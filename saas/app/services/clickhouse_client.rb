require "net/http"

# Minimal ClickHouse HTTP client for the analytics pipes.
#
# Uses the HTTP interface with server-side parameter binding
# ({name:Type} placeholders + param_<name> query args) so string parameters
# are never interpolated into SQL.
class ClickhouseClient
  class QueryError < StandardError; end

  def self.configured?
    ENV["CLICKHOUSE_URL"].present?
  end

  def self.instance
    @instance ||= new
  end

  def initialize(
    url: ENV.fetch("CLICKHOUSE_URL"),
    database: ENV.fetch("CLICKHOUSE_DATABASE", "scrapix"),
    user: ENV["CLICKHOUSE_USER"],
    password: ENV["CLICKHOUSE_PASSWORD"]
  )
    @base_uri = URI.parse(url)
    @database = database
    @user = user
    @password = password
  end

  # Runs a SELECT and returns the parsed rows (Array of Hashes).
  # `params` values are bound server-side via param_<name>.
  def query(sql, params: {})
    uri = @base_uri.dup
    query_args = { "database" => @database, "default_format" => "JSON" }
    # 64-bit integers arrive as JSON numbers (matches the Rust client's
    # serialization; ClickHouse quotes them as strings by default).
    query_args["output_format_json_quote_64bit_integers"] = "0"
    params.each { |name, value| query_args["param_#{name}"] = value.to_s }
    uri.query = URI.encode_www_form(query_args)

    http = Net::HTTP.new(uri.host, uri.port)
    http.use_ssl = uri.scheme == "https"
    http.open_timeout = 5
    http.read_timeout = 15

    request = Net::HTTP::Post.new(uri)
    request.basic_auth(@user, @password) if @user
    request["Content-Type"] = "text/plain"
    request.body = sql

    response = http.request(request)
    raise QueryError, response.body.to_s.strip unless response.is_a?(Net::HTTPSuccess)

    JSON.parse(response.body).fetch("data", [])
  rescue Errno::ECONNREFUSED, Net::OpenTimeout, Net::ReadTimeout => e
    raise QueryError, e.message
  end
end
