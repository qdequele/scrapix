require "net/http"

# Proxies requests to a registered Meilisearch engine, matching the Rust
# handlers' error mapping: connection failures and non-2xx responses surface
# as 400 bad_request with the Meilisearch status/body embedded in the message.
class MeilisearchProxy
  def self.get(engine, path)
    perform(engine, Net::HTTP::Get, path)
  end

  def self.post(engine, path, payload)
    perform(engine, Net::HTTP::Post, path) do |request|
      request["Content-Type"] = "application/json"
      request.body = payload.to_json
    end
  end

  def self.perform(engine, method_class, path)
    uri = URI.parse("#{engine.url.chomp('/')}#{path}")
    http = Net::HTTP.new(uri.host, uri.port)
    http.use_ssl = uri.scheme == "https"
    http.open_timeout = 5
    http.read_timeout = 15

    request = method_class.new(uri)
    request["Authorization"] = "Bearer #{engine.api_key}" if engine.api_key.present?
    yield request if block_given?

    response = http.request(request)
    unless response.is_a?(Net::HTTPSuccess)
      raise ApiErrorRendering::ApiError.new(
        "Meilisearch returned #{response.code}: #{response.body}", "bad_request"
      )
    end

    JSON.parse(response.body)
  rescue Errno::ECONNREFUSED, Net::OpenTimeout, Net::ReadTimeout, SocketError => e
    raise ApiErrorRendering::ApiError.new("Failed to connect to Meilisearch: #{e.message}", "bad_request")
  end
  private_class_method :perform
end
