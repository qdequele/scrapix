require "test_helper"

class McpTest < ActionDispatch::IntegrationTest
  test "requires a Bearer token with the engine's error bodies" do
    post "/mcp", params: {}, as: :json
    assert_response :unauthorized
    assert_equal({ "error" => "Missing Bearer token", "code" => "missing_token" }, response.parsed_body)

    post "/mcp", params: {}, as: :json, headers: { "Authorization" => "Bearer sxat_invalid" }
    assert_response :unauthorized
    assert_equal "invalid_token", response.parsed_body["code"]
  end

  test "lists the OpenAPI-derived tools" do
    client = OauthClient.create!(client_id: "sxc_#{'e' * 32}", redirect_uris: [ "http://localhost/cb" ])
    raw = "sxat_#{'f' * 48}"
    OauthToken.create!(
      token_hash: Digest::SHA256.hexdigest(raw), token_type: "access",
      client_id: client.client_id, user_id: users(:quentin).id, expires_at: 1.hour.from_now
    )

    post "/mcp", headers: { "Authorization" => "Bearer #{raw}" },
                 params: { jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }, as: :json
    assert_response :success
    names = response.parsed_body.dig("result", "tools").map { |t| t["name"] }
    assert_operator names.size, :>=, 40
    assert_includes names, "scrape_url"
    assert_includes names, "list_configs"
  end
end
