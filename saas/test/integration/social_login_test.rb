require "test_helper"

# Social login via rodauth-omniauth, driven with OmniAuth's test mode. The
# critical assertion: omniauth account creation goes through a SEPARATE hook
# (after_omniauth_create_account), which must provision the billing account
# exactly like password signup does.
class SocialLoginTest < ActionDispatch::IntegrationTest
  setup do
    OmniAuth.config.test_mode = true
    OmniAuth.config.mock_auth[:github] = OmniAuth::AuthHash.new(
      provider: "github", uid: "gh-12345",
      info: { email: "social@example.com", name: "Social User" }
    )
  end

  teardown do
    OmniAuth.config.test_mode = false
    OmniAuth.config.mock_auth[:github] = nil
  end

  test "first github login creates the user with a provisioned billing account" do
    assert_difference [ "User.count", "Account.count" ] do
      get "/auth/github"
      follow_redirect! # -> /auth/github/callback (test mode short-circuits)
    end

    user = User.find_by(email: "social@example.com")
    assert user.verified?, "omniauth accounts are created verified"
    assert_equal "Social User", user.full_name
    assert_equal "gh-12345", OauthIdentity.find_by(user_id: user.id, provider: "github")&.provider_user_id

    membership = user.primary_membership
    assert membership, "billing account must be provisioned for social signups"
    assert_equal "owner", membership.role
    assert_equal 100, membership.account.credits_balance
    assert cookies["scrapix_session"].present?, "engine JWT must be issued"

    # Redirected back to the console after the flow.
    assert_response :redirect
    assert_includes response.location, "/dashboard"
  end

  test "second login reuses the identity without duplicating accounts" do
    get "/auth/github"
    follow_redirect!
    post "/auth/logout", params: {}, as: :json

    assert_no_difference [ "User.count", "Account.count" ] do
      get "/auth/github"
      follow_redirect!
    end
    assert cookies["scrapix_session"].present?
  end
end
