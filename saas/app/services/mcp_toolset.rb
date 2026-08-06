# Builds the MCP tool set from the frozen OpenAPI snapshot (contracts/
# openapi.json) — the same spec-driven approach as the Rust rmcp_openapi
# server, so tool names (operationIds) and shapes carry over unchanged.
#
# Each tool proxies its call over HTTP with the caller's OAuth Bearer token:
# SaaS route groups go to this Rails app, everything else (scrape/crawl/map/
# search/jobs/diagnostics) to the Rust engine. Requires at least two Puma
# threads, since a SaaS tool call re-enters the app.
module McpToolset
  # Route groups served by Rails — keep in sync with SAAS_PREFIXES in
  # Procfile.dev / console proxy routing.
  SAAS_PREFIXES = %w[analytics configs engines auth account webhooks oauth].freeze

  class << self
    def tools
      @tools ||= build_tools
    end

    def spec_path
      ENV.fetch("MCP_OPENAPI_SPEC_PATH", Rails.root.join("../contracts/openapi.json").to_s)
    end

    private

    def build_tools
      spec = JSON.parse(File.read(spec_path))
      components = spec.dig("components", "schemas") || {}

      spec["paths"].flat_map do |path, methods|
        methods.filter_map do |http_method, op|
          next unless op.is_a?(Hash) && op["operationId"]

          build_tool(http_method.upcase, path, op, components)
        end
      end
    end

    def build_tool(http_method, path, op, components)
      params = (op["parameters"] || []).map { |p| resolve_ref(p, components) }
      path_params = params.select { |p| p["in"] == "path" }
      query_params = params.select { |p| p["in"] == "query" }

      body_schema = op.dig("requestBody", "content", "application/json", "schema")
      body_schema = deep_resolve(body_schema, components) if body_schema

      properties = {}
      required = []
      (path_params + query_params).each do |p|
        properties[p["name"]] = deep_resolve(p["schema"] || { "type" => "string" }, components)
          .merge(p["description"] ? { "description" => p["description"] } : {})
        required << p["name"] if p["in"] == "path" || p["required"]
      end
      if body_schema.is_a?(Hash) && body_schema["properties"]
        properties = body_schema["properties"].merge(properties)
        required |= Array(body_schema["required"])
      end

      description = op["summary"] || op["description"] || "#{http_method} #{path}"
      param_names = { path: path_params.map { |p| p["name"] }, query: query_params.map { |p| p["name"] } }
      has_body = !body_schema.nil?

      MCP::Tool.define(
        name: op["operationId"],
        description: description,
        input_schema: { properties: properties, required: required }
      ) do |server_context: nil, **args|
        McpToolset.execute(
          http_method: http_method, path: path, args: args,
          param_names: param_names, has_body: has_body,
          bearer: server_context&.dig(:bearer)
        )
      end
    end

    def resolve_ref(node, components)
      return node unless node.is_a?(Hash) && node["$ref"]

      name = node["$ref"].split("/").last
      components[name] || node
    end

    # Inline $refs so MCP clients see self-contained JSON Schemas (depth-capped
    # to survive recursive component definitions).
    def deep_resolve(node, components, depth = 0)
      return node if depth > 8

      case node
      when Hash
        node = resolve_ref(node, components)
        node.each_with_object({}) do |(k, v), out|
          next if k == "$ref"

          out[k] = deep_resolve(v, components, depth + 1)
        end
      when Array
        node.map { |v| deep_resolve(v, components, depth + 1) }
      else
        node
      end
    end
  end

  def self.execute(http_method:, path:, args:, param_names:, has_body:, bearer:)
    args = args.transform_keys(&:to_s)

    url_path = path.gsub(/\{(\w+)\}/) { CGI.escape(args[Regexp.last_match(1)].to_s) }
    query = param_names[:query].filter_map { |name| [ name, args[name] ] if args.key?(name) }
    body_args = args.except(*param_names[:path], *param_names[:query])

    uri = URI.join(backend_for(url_path), url_path)
    uri.query = URI.encode_www_form(query) if query.any?

    request = Net::HTTP.const_get(http_method.capitalize).new(uri)
    request["Authorization"] = "Bearer #{bearer}" if bearer
    if has_body
      request["Content-Type"] = "application/json"
      request.body = JSON.generate(body_args)
    end

    response = Net::HTTP.start(uri.hostname, uri.port, use_ssl: uri.scheme == "https", read_timeout: 120) do |http|
      http.request(request)
    end

    MCP::Tool::Response.new(
      [ { type: "text", text: response.body.to_s } ],
      error: response.code.to_i >= 400
    )
  rescue StandardError => e
    MCP::Tool::Response.new([ { type: "text", text: "Request failed: #{e.message}" } ], error: true)
  end

  def self.backend_for(path)
    prefix = path.delete_prefix("/").split("/").first
    if SAAS_PREFIXES.include?(prefix)
      ENV.fetch("MCP_SAAS_BASE_URL", "http://localhost:#{ENV.fetch('SAAS_PORT', '8081')}")
    else
      ENV.fetch("SCRAPIX_ENGINE_URL", "http://localhost:8080")
    end
  end
end
