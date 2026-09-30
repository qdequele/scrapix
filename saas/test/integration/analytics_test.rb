require "test_helper"

class AnalyticsTest < ActionDispatch::IntegrationTest
  ACME_KEY = "sk_live_TEST-KEY-TEST-KEY-TEST-KEY-TEST".freeze

  PIPE_PATHS = %w[
    pipes
    pipes/top_domains.json
    pipes/domain_stats.json?domain=example.com
    pipes/hourly_stats.json
    pipes/daily_stats.json
    pipes/error_distribution.json
    pipes/job_stats.json?job_id=job_1
    pipes/kpis.json
    pipes/ai_usage.json
    pipes/job_timeline.json?job_id=job_1
    pipes/job_event_summary.json?job_id=job_1
    pipes/account_usage.json
    pipes/account_daily_usage.json
    pipes/account_daily_usage_by_operation.json
    pipes/api_key_usage.json
  ].freeze

  # Records every query instead of calling ClickHouse; returns no rows.
  class FakeClickhouse
    attr_reader :queries

    def initialize = @queries = []

    def query(sql, params: {})
      @queries << [ sql, params ]
      []
    end
  end

  setup do
    @previous_url = ENV["CLICKHOUSE_URL"]
    ENV["CLICKHOUSE_URL"] = "http://clickhouse.test:8123"
    @previous_instance = ClickhouseClient.instance_variable_get(:@instance)
    @fake = FakeClickhouse.new
    ClickhouseClient.instance_variable_set(:@instance, @fake)
  end

  teardown do
    ClickhouseClient.instance_variable_set(:@instance, @previous_instance)
    ENV["CLICKHOUSE_URL"] = @previous_url
  end

  test "every pipe refuses unauthenticated requests" do
    PIPE_PATHS.each do |path|
      get "/analytics/v0/#{path}"
      assert_response :unauthorized, path
    end
    assert_empty @fake.queries
  end

  test "every query is scoped to the session's account" do
    sign_in_as users(:quentin), account: accounts(:acme)
    PIPE_PATHS.drop(1).each do |path|
      @fake.queries.clear
      get "/analytics/v0/#{path}", headers: auth_headers
      assert_response :success, path
      assert_not_empty @fake.queries, path
      @fake.queries.each do |sql, bind|
        assert_equal accounts(:acme).id, bind[:account_id], path
        assert_match(/account_id = \{account_id:String\}/, sql, path)
      end
    end
  end

  test "an API key is scoped to its own account" do
    get "/analytics/v0/pipes/kpis.json", headers: { "X-API-Key" => ACME_KEY }
    assert_response :success
    assert_equal [ accounts(:acme).id ], @fake.queries.map { |_, bind| bind[:account_id] }
  end

  test "an account_id naming another account is refused" do
    sign_in_as users(:quentin), account: accounts(:acme)
    %w[account_usage account_daily_usage account_daily_usage_by_operation api_key_usage ai_usage].each do |pipe|
      get "/analytics/v0/pipes/#{pipe}.json?account_id=#{accounts(:globex).id}", headers: auth_headers
      assert_response :not_found, pipe
    end
    assert_empty @fake.queries
  end

  test "an account_id naming the caller's own account still works" do
    sign_in_as users(:quentin), account: accounts(:acme)
    get "/analytics/v0/pipes/account_usage.json?account_id=#{accounts(:acme).id}", headers: auth_headers
    assert_response :success
    assert_equal accounts(:acme).id, response.parsed_body["data"].first["account_id"]
  end

  test "a session cannot select an account it is not a member of" do
    sign_in_as users(:outsider), account: accounts(:acme)
    get "/analytics/v0/pipes/kpis.json", headers: auth_headers
    assert_response :not_found
    assert_empty @fake.queries
  end
end
