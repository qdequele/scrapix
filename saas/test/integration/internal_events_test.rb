require "test_helper"

class InternalEventsTest < ActionDispatch::IntegrationTest
  include ActiveJob::TestHelper

  SECRET = "0123456789abcdef0123456789abcdef"
  setup { @prev = ENV["LAB_EVENTS_SECRET"]; ENV["LAB_EVENTS_SECRET"] = SECRET }
  teardown { ENV["LAB_EVENTS_SECRET"] = @prev }

  def event(id: SecureRandom.uuid, type: "usage.recorded", account: accounts(:acme).id)
    { "id" => id, "type" => type, "occurred_at" => Time.current.utc.iso8601(3), "account_id" => account,
      "api_key_id" => nil, "product" => "scrapix",
      "data" => { "operation" => "scrape", "credits" => 3, "units" => {}, "description" => "https://e.com (3 credits)" } }
  end

  def post_raw(body, signature: "sha256=#{OpenSSL::HMAC.hexdigest('SHA256', SECRET, body)}")
    post "/internal/events", params: body, headers: { "Content-Type" => "application/json", "X-Scrapix-Signature" => signature }
  end

  test "accepts signed events and stores them once" do
    e = event
    body = { events: [ e ] }.to_json
    assert_enqueued_with(job: ProcessLabEventsJob) { post_raw(body) }
    assert_response :success
    assert_equal [ e["id"] ], response.parsed_body["accepted"]
    post_raw(body)
    assert_equal [ e["id"] ], response.parsed_body["accepted"], "duplicates are acknowledged too"
    assert_equal 1, LabEventReceived.where(id: e["id"]).count
    row = LabEventReceived.find(e["id"])
    assert_equal accounts(:acme).id, row.account_id
    assert_equal "usage.recorded", row.type
    assert_in_delta Time.iso8601(e["occurred_at"]), row.occurred_at, 0.001
    assert_equal e, row.payload
    assert_nil row.processed_at
  end

  test "signature_is_checked_on_raw_body" do
    body = JSON.pretty_generate({ events: [ event ] }) # different whitespace than to_json
    post_raw(body)
    assert_response :success
    post_raw(body.sub("3 credits", "4 credits"), signature: "sha256=#{OpenSSL::HMAC.hexdigest('SHA256', SECRET, body)}")
    assert_response :unauthorized
  end

  test "missing or wrong signature is rejected" do
    body = { events: [ event ] }.to_json
    post "/internal/events", params: body, headers: { "Content-Type" => "application/json" }
    assert_response :unauthorized
    post_raw(body, signature: "sha256=deadbeef")
    assert_response :unauthorized
    assert_equal 0, LabEventReceived.count
    assert_no_enqueued_jobs
  end

  test "malformed events are skipped without failing the batch" do
    good = event
    bad = event.merge("id" => "not-a-uuid")
    post_raw({ events: [ good, bad, { "id" => SecureRandom.uuid } ] }.to_json)
    assert_response :success
    assert_equal [ good["id"] ], response.parsed_body["accepted"]
    assert_equal [ good["id"] ], LabEventReceived.pluck(:id)
  end

  test "each skipped event is logged with its id and the reason" do
    bad_id = event.merge("id" => "not-a-uuid")
    bad_account = event.merge("account_id" => "nope")
    no_type = event.tap { |e| e.delete("type") }
    bad_time = event.merge("occurred_at" => "yesterday")
    io = StringIO.new
    prev = Rails.logger
    Rails.logger = ActiveSupport::Logger.new(io)
    begin
      post_raw({ events: [ bad_id, bad_account, no_type, bad_time, "junk" ] }.to_json)
    ensure
      Rails.logger = prev
    end
    assert_response :success
    assert_equal [], response.parsed_body["accepted"]
    log = io.string
    assert_match(/not-a-uuid.*invalid id/, log)
    assert_match(/#{bad_account["id"]}.*invalid account_id/, log)
    assert_match(/#{no_type["id"]}.*missing type/, log)
    assert_match(/#{bad_time["id"]}.*invalid occurred_at/, log)
    assert_match(/event is not an object/, log)
    assert_equal 0, LabEventReceived.count
    assert_no_enqueued_jobs
  end

  test "correctly signed body that is not valid JSON answers 400" do
    post_raw("{not json")
    assert_response :bad_request
    post_raw("[]") # valid JSON, wrong shape
    assert_response :bad_request
    assert_equal 0, LabEventReceived.count
    assert_no_enqueued_jobs
  end

  test "unconfigured secret answers 503" do
    ENV["LAB_EVENTS_SECRET"] = nil
    post_raw({ events: [] }.to_json, signature: "sha256=x")
    assert_response :service_unavailable
    assert_equal 0, LabEventReceived.count
    assert_no_enqueued_jobs
  end
end
