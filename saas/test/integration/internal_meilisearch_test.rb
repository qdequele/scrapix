require "test_helper"

class InternalMeilisearchTest < ActionDispatch::IntegrationTest
  TOKEN = "t" * 40
  setup { @previous = ENV["LAB_SERVICE_TOKEN"]; ENV["LAB_SERVICE_TOKEN"] = TOKEN }
  teardown { ENV["LAB_SERVICE_TOKEN"] = @previous }

  def lookup(account, url: nil)
    get "/internal/accounts/#{account.id}/meilisearch", params: { url: url }.compact,
                                                          headers: { "Authorization" => "Bearer #{TOKEN}" }
  end

  test "default engine with its decrypted key" do
    engine = MeilisearchEngine.create!(account: accounts(:globex), name: "prod", url: "http://m:7700",
                                       api_key: "secret-key-1234", is_default: true)
    lookup(accounts(:globex))
    assert_response :success
    assert_equal({ "id" => engine.id, "url" => "http://m:7700", "api_key" => "secret-key-1234" }, response.parsed_body)
  end

  test "lookup by url ignores a trailing slash; unknown url or no default is 404" do
    MeilisearchEngine.create!(account: accounts(:globex), name: "b", url: "http://b:7700/", api_key: "kb")
    lookup(accounts(:globex), url: "http://b:7700")
    assert_equal "kb", response.parsed_body["api_key"]
    lookup(accounts(:globex), url: "http://nope:7700")
    assert_response :not_found
    lookup(accounts(:globex))
    assert_response :not_found
  end

  test "requires the service token" do
    get "/internal/accounts/#{accounts(:acme).id}/meilisearch"
    assert_response :unauthorized
  end

  test "the key is encrypted at rest" do
    engine = MeilisearchEngine.create!(account: accounts(:globex), name: "enc", url: "http://e:7700", api_key: "plain-secret")
    raw = MeilisearchEngine.connection.select_value(
      MeilisearchEngine.sanitize_sql_array([ "SELECT api_key FROM meilisearch_engines WHERE id = ?", engine.id ])
    )
    assert_not_includes raw, "plain-secret"
  end
end
