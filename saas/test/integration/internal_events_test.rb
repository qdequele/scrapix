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
  end

  test "malformed events are skipped without failing the batch" do
    good = event
    bad = event.merge("id" => "not-a-uuid")
    post_raw({ events: [ good, bad, { "id" => SecureRandom.uuid } ] }.to_json)
    assert_response :success
    assert_equal [ good["id"] ], response.parsed_body["accepted"]
  end

  test "unconfigured secret answers 503" do
    ENV["LAB_EVENTS_SECRET"] = nil
    post_raw({ events: [] }.to_json, signature: "sha256=x")
    assert_response :service_unavailable
  end
end
