require "test_helper"

class ApiKeysTest < ActionDispatch::IntegrationTest
  setup { sign_in_as users(:quentin) }

  test "lists the account's keys" do
    get "/account/api-keys"
    assert_response :success
    assert_equal [ "CI key" ], response.parsed_body.map { |k| k["name"] }
  end

  test "creates a key and returns the raw secret once" do
    post "/account/api-keys", params: { name: "deploy" }, as: :json
    assert_response :success
    body = response.parsed_body
    assert_match(/\Ask_live_[A-Za-z0-9]{32}\z/, body["key"])
    assert_equal "#{body['key'][0, 12]}...", body["prefix"]
    assert ApiKey.exists?(key_hash: Digest::SHA256.hexdigest(body["key"]))
  end

  test "revokes a key" do
    patch "/account/api-keys/#{api_keys(:acme_key).id}"
    assert_response :success
    assert_not api_keys(:acme_key).reload.active
  end

  test "members cannot create keys" do
    sign_in_as users(:teammate)
    post "/account/api-keys", params: { name: "nope" }, as: :json
    assert_response :forbidden
  end

  test "rejects requests without a session" do
    cookies.delete("scrapix_session")
    get "/account/api-keys"
    assert_response :unauthorized
    assert_equal "not_authenticated", response.parsed_body["code"]
  end
end
