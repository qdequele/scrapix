require "test_helper"

# The receiver must accept exactly what contracts/lab-events.schema.json
# describes (the Rust engine's tests validate its serialized events against
# the same file). No JSON Schema gem: a tiny checker covers the keywords the
# contract uses (type, required, properties, additionalProperties, enum,
# const, minimum, pattern, $ref) — it fails loudly on any other keyword.
class LabEventsContractTest < ActionDispatch::IntegrationTest
  include ActiveJob::TestHelper

  SECRET = "0123456789abcdef0123456789abcdef"
  SCHEMA = JSON.parse(File.read(Rails.root.join("../contracts/lab-events.schema.json")))
  DATA_DEFS = { "usage.recorded" => "usageData", "job.completed" => "jobCompletedData", "job.failed" => "jobFailedData" }.freeze
  SUPPORTED = %w[type required properties additionalProperties enum const minimum pattern $ref description format].freeze

  setup { @prev = ENV["LAB_EVENTS_SECRET"]; ENV["LAB_EVENTS_SECRET"] = SECRET }
  teardown { ENV["LAB_EVENTS_SECRET"] = @prev }

  def envelope(type, data, account: accounts(:acme).id)
    { "id" => SecureRandom.uuid, "type" => type, "occurred_at" => Time.current.utc.iso8601(3),
      "account_id" => account, "api_key_id" => nil, "product" => "scrapix", "data" => data }
  end

  def fixtures_for_contract
    [
      envelope("usage.recorded", { "operation" => "scrape", "credits" => 3, "units" => { "formats" => [ "markdown" ] },
                                   "description" => "https://e.com (3 credits)" }),
      envelope("usage.recorded", { "operation" => "crawl", "credits" => 12, "units" => { "pages_http" => 12 },
                                   "description" => "Job j-1 (12 http)", "job_id" => "j-1" }),
      envelope("job.completed", { "job_id" => "j-1", "index_uid" => "docs", "pages_crawled" => 12,
                                  "documents_indexed" => 12, "duration_secs" => 30 }),
      envelope("job.failed", { "job_id" => "j-2", "error_message" => "boom", "pages_crawled" => 0 })
    ]
  end

  def resolve(schema)
    schema.key?("$ref") ? SCHEMA.dig(*schema["$ref"].delete_prefix("#/").split("/")) : schema
  end

  def json_type?(value, type)
    case type
    when "string" then value.is_a?(String)
    when "integer" then value.is_a?(Integer)
    when "object" then value.is_a?(Hash)
    when "null" then value.nil?
    else raise "unsupported type #{type}"
    end
  end

  def check(value, schema, path = "$")
    errors = []
    schema = resolve(schema)
    unsupported = schema.keys - SUPPORTED
    raise "checker does not support #{unsupported} at #{path}" if unsupported.any?

    if (t = schema["type"]) && Array(t).none? { |x| json_type?(value, x) }
      return [ "#{path}: expected #{t}, got #{value.inspect.truncate(60)}" ]
    end
    errors << "#{path}: expected #{schema['const'].inspect}" if schema.key?("const") && value != schema["const"]
    errors << "#{path}: #{value.inspect} not in #{schema['enum']}" if schema["enum"] && !schema["enum"].include?(value)
    errors << "#{path}: below minimum" if schema["minimum"] && value.is_a?(Integer) && value < schema["minimum"]
    errors << "#{path}: pattern mismatch" if schema["pattern"] && value.is_a?(String) && !value.match?(Regexp.new(schema["pattern"]))
    if value.is_a?(Hash)
      (schema["required"] || []).each { |k| errors << "#{path}: missing #{k}" unless value.key?(k) }
      props = schema["properties"] || {}
      errors << "#{path}: unexpected #{(value.keys - props.keys)}" if schema["additionalProperties"] == false && (value.keys - props.keys).any?
      props.each { |k, sub| errors.concat(check(value[k], sub, "#{path}.#{k}")) if value.key?(k) }
    end
    errors
  end

  test "contract fixtures satisfy the schema, and the checker rejects bad ones" do
    envelope_schema = SCHEMA.slice("type", "required", "additionalProperties", "properties")
    fixtures_for_contract.each do |e|
      errors = check(e, envelope_schema) + check(e["data"], { "$ref" => "#/$defs/#{DATA_DEFS.fetch(e['type'])}" }, "$.data")
      assert_empty errors, e.to_json
    end
    bad = fixtures_for_contract.first
    assert check(bad.merge("product" => "other"), envelope_schema).any?
    assert check(bad.except("api_key_id"), envelope_schema).any?
    assert check(bad["data"].merge("credits" => -1), { "$ref" => "#/$defs/usageData" }).any?
    assert check(bad["data"].merge("operation" => "teleport"), { "$ref" => "#/$defs/usageData" }).any?
  end

  test "the receiver accepts every contract event type" do
    events = fixtures_for_contract
    body = { events: events }.to_json
    post "/internal/events", params: body,
         headers: { "Content-Type" => "application/json",
                    "X-Scrapix-Signature" => "sha256=#{OpenSSL::HMAC.hexdigest('SHA256', SECRET, body)}" }
    assert_response :success
    assert_equal events.map { |e| e["id"] }, response.parsed_body["accepted"]
    events.each { |e| assert_equal e, LabEventReceived.find(e["id"]).payload }
  end
end
