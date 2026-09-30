require "test_helper"

class InternalApiTest < ActionDispatch::IntegrationTest
  TOKEN = "t" * 40
  ACME_KEY = "sk_live_TEST-KEY-TEST-KEY-TEST-KEY-TEST".freeze

  setup { @previous = ENV["LAB_SERVICE_TOKEN"]; ENV["LAB_SERVICE_TOKEN"] = TOKEN }
  teardown { ENV["LAB_SERVICE_TOKEN"] = @previous }

  def service = { "Authorization" => "Bearer #{TOKEN}" }

  def introspect(kind, credential, account_id: nil)
    post "/internal/auth/introspect", params: { kind: kind, credential: credential, account_id: account_id }.compact,
                                      headers: service, as: :json
    response.parsed_body
  end

  test "every internal endpoint needs the service token" do
    get "/internal/ping"
    assert_response :unauthorized
    get "/internal/ping", headers: { "Authorization" => "Bearer wrong" }
    assert_response :unauthorized
    post "/internal/auth/introspect", params: { kind: "api_key", credential: ACME_KEY }, as: :json
    assert_response :unauthorized
    get "/internal/accounts/#{accounts(:acme).id}"
    assert_response :unauthorized
  end

  test "an unset LAB_SERVICE_TOKEN refuses everything" do
    ENV["LAB_SERVICE_TOKEN"] = ""
    get "/internal/ping", headers: { "Authorization" => "Bearer " }
    assert_response :unauthorized
  end

  test "ping" do
    get "/internal/ping", headers: service
    assert_response :success
    assert_equal({ "ok" => true }, response.parsed_body)
  end

  test "an API key introspects to its account" do
    body = introspect("api_key", ACME_KEY)
    assert body["active"]
    assert_equal accounts(:acme).id, body["account_id"]
    assert_equal "free", body["tier"]
    assert_equal api_keys(:acme_key).id, body["api_key_id"]
    assert_nil body["role"]
    assert_equal({ "type" => "api_key", "user_id" => nil }, body["principal"])
    assert_equal 100, body.dig("credits", "balance")
    assert_equal 30, body["cache_ttl"]
  end

  test "an unknown, malformed or revoked API key is inactive" do
    assert_equal({ "active" => false }, introspect("api_key", "sk_live_nope"))
    assert_equal({ "active" => false }, introspect("api_key", "not-a-key"))
    api_keys(:acme_key).update!(active: false)
    assert_equal({ "active" => false }, introspect("api_key", ACME_KEY))
  end

  test "an inactive account is inactive" do
    accounts(:acme).update!(active: false)
    assert_equal({ "active" => false }, introspect("api_key", ACME_KEY))
  end

  test "a session introspects to the selected membership with its role" do
    token = SessionToken.encode(users(:quentin).id, users(:quentin).email)
    body = introspect("session", token, account_id: accounts(:acme).id)
    assert body["active"]
    assert_equal accounts(:acme).id, body["account_id"]
    assert_equal "owner", body["role"]
    assert_equal({ "type" => "session", "user_id" => users(:quentin).id }, body["principal"])
  end

  test "a session without account_id uses the first membership" do
    token = SessionToken.encode(users(:quentin).id, users(:quentin).email)
    assert_equal accounts(:acme).id, introspect("session", token)["account_id"]
  end

  test "a session selecting an account it is not a member of is inactive" do
    token = SessionToken.encode(users(:outsider).id, users(:outsider).email)
    assert_equal({ "active" => false }, introspect("session", token, account_id: accounts(:acme).id))
  end

  test "an expired or forged session is inactive" do
    assert_equal({ "active" => false }, introspect("session", "not.a.jwt"))
    forged = JWT.encode({ sub: users(:quentin).id, exp: 1.hour.from_now.to_i }, "other-secret", "HS256")
    assert_equal({ "active" => false }, introspect("session", forged))
  end

  test "an OAuth access token introspects; revoked or expired ones don't" do
    client = OauthClient.create!(client_id: "sxc_#{'e' * 32}", redirect_uris: [ "http://localhost/cb" ])
    raw = "sxat_#{'f' * 48}"
    token = OauthToken.create!(token_hash: Digest::SHA256.hexdigest(raw), token_type: "access",
                               client_id: client.client_id, user_id: users(:quentin).id, expires_at: 1.hour.from_now)
    body = introspect("bearer", raw)
    assert body["active"]
    assert_equal accounts(:acme).id, body["account_id"]
    assert_equal({ "type" => "oauth", "user_id" => users(:quentin).id }, body["principal"])
    token.update!(revoked: true)
    assert_equal({ "active" => false }, introspect("bearer", raw))
    token.update!(revoked: false, expires_at: 1.minute.ago)
    assert_equal({ "active" => false }, introspect("bearer", raw))
  end

  test "a session or OAuth token is never cached past its own expiry" do
    soon = JWT.encode({ sub: users(:quentin).id, email: users(:quentin).email, exp: 10.seconds.from_now.to_i },
                      ENV.fetch("JWT_SECRET"), "HS256")
    assert_includes 9..10, introspect("session", soon)["cache_ttl"]
    later = SessionToken.encode(users(:quentin).id, users(:quentin).email)
    assert_equal 30, introspect("session", later)["cache_ttl"]

    client = OauthClient.create!(client_id: "sxc_#{'c' * 32}", redirect_uris: [ "http://localhost/cb" ])
    raw = "sxat_#{'d' * 48}"
    token = OauthToken.create!(token_hash: Digest::SHA256.hexdigest(raw), token_type: "access",
                               client_id: client.client_id, user_id: users(:quentin).id, expires_at: 5.seconds.from_now)
    assert_includes 4..5, introspect("bearer", raw)["cache_ttl"]
    token.update!(expires_at: 1.hour.from_now)
    assert_equal 30, introspect("bearer", raw)["cache_ttl"]
    token.update!(expires_at: Time.current + 0.5)
    assert_equal 0, introspect("bearer", raw)["cache_ttl"], "floored, never negative"
  end

  test "an unknown kind or a missing credential is a 400" do
    post "/internal/auth/introspect", params: { kind: "magic", credential: "x" }, headers: service, as: :json
    assert_response :bad_request
    post "/internal/auth/introspect", params: { kind: "api_key" }, headers: service, as: :json
    assert_response :bad_request
  end

  test "the balance excludes usage received but not yet debited" do
    LabEventReceived.create!(id: SecureRandom.uuid, type: "usage.recorded", account_id: accounts(:acme).id,
                             occurred_at: Time.current,
                             payload: { "data" => { "operation" => "scrape", "credits" => 30 } })
    LabEventReceived.create!(id: SecureRandom.uuid, type: "usage.recorded", account_id: accounts(:acme).id,
                             occurred_at: Time.current, processed_at: Time.current,
                             payload: { "data" => { "operation" => "scrape", "credits" => 999 } })
    assert_equal 70, introspect("api_key", ACME_KEY).dig("credits", "balance")
  end

  test "account lookup for service calls" do
    get "/internal/accounts/#{accounts(:globex).id}", headers: service
    assert_response :success
    assert_equal({ "active" => true, "account_id" => accounts(:globex).id, "tier" => "pro",
                   "credits" => { "balance" => 5000 }, "cache_ttl" => 30 }, response.parsed_body)
    get "/internal/accounts/#{SecureRandom.uuid}", headers: service
    assert_equal({ "active" => false }, response.parsed_body)
    get "/internal/accounts/not-a-uuid", headers: service
    assert_equal({ "active" => false }, response.parsed_body)
  end

  test "the request log never carries the introspected credential" do
    filter = ActiveSupport::ParameterFilter.new(Rails.application.config.filter_parameters)
    assert_equal({ "credential" => "[FILTERED]", "kind" => "api_key" },
                 filter.filter("credential" => ACME_KEY, "kind" => "api_key"))
  end
end
