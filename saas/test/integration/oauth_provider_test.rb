require "test_helper"

# Full OAuth 2.1 PKCE flow against the provider endpoints. The password step
# is covered by the contract suite (it needs a real signup flow); here we
# exercise registration, token exchange, rotation, and revocation.
class OauthProviderTest < ActionDispatch::IntegrationTest
  REDIRECT = "http://localhost:9999/callback".freeze

  test "authorization-server metadata is discoverable" do
    get "/.well-known/oauth-authorization-server"
    assert_response :success
    body = response.parsed_body
    assert_equal "#{body['issuer']}/oauth/token", body["token_endpoint"]
    assert_equal [ "S256" ], body["code_challenge_methods_supported"]
  end

  test "protected-resource metadata points at the authorization server" do
    get "/.well-known/oauth-protected-resource"
    assert_response :success
    assert_equal [ response.parsed_body["authorization_servers"].first ],
                  response.parsed_body["authorization_servers"]
  end

  test "dynamic client registration validates redirect uris" do
    post "/oauth/register", params: { redirect_uris: [] }, as: :json
    assert_response :bad_request
    assert_equal "invalid_client_metadata", response.parsed_body["error"]

    post "/oauth/register", params: { client_name: "t", redirect_uris: [ REDIRECT ] }, as: :json
    assert_response :success
    assert_match(/\Asxc_[A-Za-z0-9]{32}\z/, response.parsed_body["client_id"])
  end

  test "code exchange verifies PKCE and single use; refresh rotates" do
    client = OauthClient.create!(client_id: "sxc_#{'c' * 32}", redirect_uris: [ REDIRECT ])
    verifier = SecureRandom.urlsafe_base64(32)
    challenge = Base64.urlsafe_encode64(Digest::SHA256.digest(verifier), padding: false)
    code = "sxac_#{'d' * 48}"
    OauthAuthorizationCode.create!(
      code: code, client_id: client.client_id, user_id: users(:quentin).id,
      redirect_uri: REDIRECT, code_challenge: challenge, code_challenge_method: "S256",
      expires_at: 10.minutes.from_now
    )

    post "/oauth/token", params: {
      grant_type: "authorization_code", code: code, code_verifier: "wrong",
      redirect_uri: REDIRECT, client_id: client.client_id
    }
    assert_response :bad_request
    assert_equal "PKCE verification failed", response.parsed_body["error_description"]

    post "/oauth/token", params: {
      grant_type: "authorization_code", code: code, code_verifier: verifier,
      redirect_uri: REDIRECT, client_id: client.client_id
    }
    assert_response :success
    tokens = response.parsed_body
    assert_match(/\Asxat_/, tokens["access_token"])

    # Single use
    post "/oauth/token", params: {
      grant_type: "authorization_code", code: code, code_verifier: verifier,
      redirect_uri: REDIRECT, client_id: client.client_id
    }
    assert_response :bad_request
    assert_equal "Authorization code already used", response.parsed_body["error_description"]

    # Refresh rotation
    post "/oauth/token", params: { grant_type: "refresh_token", refresh_token: tokens["refresh_token"] }
    assert_response :success
    rotated = response.parsed_body

    post "/oauth/token", params: { grant_type: "refresh_token", refresh_token: tokens["refresh_token"] }
    assert_response :bad_request
    assert_equal "Refresh token has been revoked", response.parsed_body["error_description"]

    # Revocation is always 200 and kills the token
    post "/oauth/revoke", params: { token: rotated["access_token"] }
    assert_response :success
    assert_nil OauthToken.account_for(rotated["access_token"])
  end
end
