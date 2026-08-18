require "test_helper"

class OauthTokenTest < ActiveSupport::TestCase
  setup do
    @client = OauthClient.create!(client_id: "sxc_#{'a' * 32}", redirect_uris: [ "http://localhost/cb" ])
    @raw = "sxat_#{'b' * 48}"
  end

  def issue!(user:, revoked: false, expires_at: 1.hour.from_now)
    OauthToken.create!(
      token_hash: Digest::SHA256.hexdigest(@raw), token_type: "access",
      client_id: @client.client_id, user_id: user.id,
      expires_at: expires_at, revoked: revoked
    )
  end

  test "account_for resolves a live token to the holder's account" do
    issue!(user: users(:quentin))
    holder = OauthToken.account_for(@raw)

    assert_equal accounts(:acme).id, holder[:account_id]
    assert_equal "free", holder[:tier]
  end

  test "account_for rejects revoked, expired, and unknown tokens" do
    issue!(user: users(:quentin), revoked: true)
    assert_nil OauthToken.account_for(@raw)

    OauthToken.delete_all
    issue!(user: users(:quentin), expires_at: 1.minute.ago)
    assert_nil OauthToken.account_for(@raw)

    assert_nil OauthToken.account_for("sxat_nope")
  end

  test "account_for requires an account membership" do
    loner = User.create!(email: "loner@example.com", password_hash: "x")
    issue!(user: loner)
    assert_nil OauthToken.account_for(@raw)
  end
end
