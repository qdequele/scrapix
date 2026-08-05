require "net/http"

# HTTP client for the Rust crawl engine's product API (SCR-85 split: Rails is
# the control plane, the engine keeps /crawl and friends). Calls forward the
# original caller's credentials so job attribution and billing context are
# preserved.
class ScrapixEngine
  class EngineError < StandardError
    attr_reader :status, :body

    def initialize(status, body)
      super("engine returned #{status}")
      @status = status
      @body = body
    end
  end

  def self.base_url
    ENV.fetch("SCRAPIX_ENGINE_URL", "http://localhost:8080")
  end

  # POST /crawl with a stored crawl config. Returns the parsed JSON response.
  def self.create_crawl(config, credentials)
    uri = URI.parse("#{base_url}/crawl")
    http = Net::HTTP.new(uri.host, uri.port)
    http.use_ssl = uri.scheme == "https"
    http.open_timeout = 5
    http.read_timeout = 30

    request = Net::HTTP::Post.new(uri)
    request["Content-Type"] = "application/json"
    request["X-API-Key"] = credentials[:api_key] if credentials[:api_key].present?
    request["Authorization"] = credentials[:bearer] if credentials[:bearer].present?
    request["X-Account-Id"] = credentials[:account_id] if credentials[:account_id].present?
    if credentials[:session_cookie].present?
      request["Cookie"] = "scrapix_session=#{credentials[:session_cookie]}"
    end
    request.body = config.to_json

    response = http.request(request)
    body = JSON.parse(response.body) rescue {}
    raise EngineError.new(response.code.to_i, body) unless response.is_a?(Net::HTTPSuccess)

    body
  end
end
