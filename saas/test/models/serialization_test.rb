require "test_helper"

# The as_json shapes are the wire contract (contracts/src/shapes.ts).
class SerializationTest < ActiveSupport::TestCase
  ISO8601 = /\A\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z\z/

  test "api key shape" do
    json = api_keys(:acme_key).as_json
    assert_equal %i[id name prefix active last_used_at created_at], json.keys
    assert_match ISO8601, json[:created_at]
    assert_nil json[:last_used_at]
  end

  test "crawl config shape" do
    json = crawl_configs(:daily_docs).as_json
    assert_equal %i[id account_id name description config cron_expression cron_enabled
                    last_run_at next_run_at last_job_id created_at updated_at], json.keys
    assert_match ISO8601, json[:updated_at]
  end

  test "engine shape" do
    json = meilisearch_engines(:acme_default).as_json
    assert_equal %i[id account_id name url api_key is_default created_at updated_at], json.keys
  end

  test "invite shape" do
    json = account_invites(:pending_invite).as_json
    assert_equal %i[id email role status invited_by expires_at created_at], json.keys
  end

  test "transaction shape" do
    json = transactions(:acme_initial).as_json
    assert_equal %i[id type amount balance_after description created_at], json.keys
  end
end
