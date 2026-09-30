require "test_helper"

class CleanupOauthTokensJobTest < ActiveJob::TestCase
  test "removes expired codes and tokens, keeps live ones" do
    user = users(:quentin)
    OauthClient.create!(client_id: "c1", redirect_uris: [ "http://localhost/cb" ])
    code_attrs = { client_id: "c1", user_id: user.id, redirect_uri: "http://localhost/cb", code_challenge: "x" }
    OauthAuthorizationCode.create!(code_attrs.merge(code: "old-code", expires_at: 2.hours.ago))
    OauthAuthorizationCode.create!(code_attrs.merge(code: "live-code", expires_at: 5.minutes.from_now))

    token_attrs = { client_id: "c1", user_id: user.id, token_type: "access" }
    OauthToken.create!(token_attrs.merge(token_hash: "expired", expires_at: 8.days.ago))
    OauthToken.create!(token_attrs.merge(token_hash: "revoked", expires_at: 1.day.from_now,
                                         revoked: true, created_at: 8.days.ago))
    OauthToken.create!(token_attrs.merge(token_hash: "live", expires_at: 1.hour.from_now))

    CleanupOauthTokensJob.perform_now

    assert_equal [ "live-code" ], OauthAuthorizationCode.pluck(:code)
    assert_equal [ "live" ], OauthToken.pluck(:token_hash)
  end
end
