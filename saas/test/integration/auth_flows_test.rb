require "test_helper"

# End-to-end Rodauth flows through the JSON API, including the email round
# trips (tokens are HMAC'd, so they can only be exercised via the real
# mailer deliveries) and the engine session bridge (scrapix_session JWT).
class AuthFlowsTest < ActionDispatch::IntegrationTest
  include ActiveJob::TestHelper

  EMAIL = "flow@example.com".freeze
  PASSWORD = "a-long-password-123".freeze

  def json_post(path, body)
    post path, params: body, as: :json, headers: { "Accept" => "application/json" }
  end

  def last_email_body
    perform_enqueued_jobs
    ActionMailer::Base.deliveries.last.body.encoded.gsub("=\r\n", "").gsub("=3D", "=")
  end

  def signup!
    json_post "/auth/signup", { email: EMAIL, password: PASSWORD, full_name: "Flow" }
    assert_response :success
  end

  test "signup creates the user, billing account, and engine session" do
    assert_difference [ "User.count", "Account.count" ] do
      signup!
    end
    assert cookies["scrapix_session"].present?

    get "/auth/me"
    body = response.parsed_body
    assert_equal EMAIL, body["email"]
    assert_equal false, body["email_verified"]
    assert_equal "Flow's Account", body.dig("account", "name")
    assert_equal 100, body.dig("account", "credits_balance")
  end

  test "signup auto-accepts live invites for the email" do
    AccountInvite.issue!(
      account_id: accounts(:globex).id, email: EMAIL, role: "member",
      invited_by: users(:outsider).id, token_hash: "x"
    )
    signup!
    get "/auth/me/accounts"
    names = response.parsed_body.map { |a| a["name"] }
    assert_includes names, "Globex"
  end

  test "signup rejects short passwords and duplicate emails" do
    json_post "/auth/signup", { email: EMAIL, password: "short" }
    assert_response :unprocessable_entity
    assert_equal "password", response.parsed_body["field-error"].first

    signup!
    json_post "/auth/signup", { email: EMAIL, password: PASSWORD }
    assert_response :forbidden # unverified duplicate: "awaiting verification"
  end

  test "verification email round trip flips status and queues the welcome email" do
    signup!
    body = last_email_body
    token = body[/verify-email\?token=([A-Za-z0-9_-]+)/, 1]
    assert token, "verification token not found in email"

    json_post "/auth/verify-email", { key: token }
    assert_response :success
    assert User.find_by(email: EMAIL).verified?

    perform_enqueued_jobs
    assert_equal "Welcome to Scrapix — your 100 free credits are ready",
                 ActionMailer::Base.deliveries.last.subject
  end

  test "login and logout manage the engine session cookie" do
    signup!
    json_post "/auth/logout", {}
    assert_response :success

    get "/auth/me"
    assert_response :unauthorized

    json_post "/auth/login", { email: EMAIL, password: PASSWORD }
    assert_response :success
    get "/auth/me"
    assert_response :success

    json_post "/auth/login", { email: EMAIL, password: "wrong-password-000" }
    assert_response :unauthorized
    assert_equal "invalid password", response.parsed_body["field-error"].last
  end

  test "password reset round trip" do
    signup!
    # Rodauth only redeems reset tokens for verified accounts.
    User.find_by(email: EMAIL).update!(status: 2)
    json_post "/auth/logout", {}

    json_post "/auth/forgot-password", { email: EMAIL }
    assert_response :success
    token = last_email_body[/reset-password\?token=([A-Za-z0-9_-]+)/, 1]
    assert token, "reset token not found in email"

    new_password = "a-brand-new-password-456"
    json_post "/auth/reset-password", { key: token, password: new_password }
    assert_response :success
    perform_enqueued_jobs
    assert_equal "Your password has been changed — Scrapix",
                 ActionMailer::Base.deliveries.last.subject

    json_post "/auth/login", { email: EMAIL, password: new_password }
    assert_response :success
  end

  test "TOTP setup and two-factor login" do
    signup!

    # Initiating setup returns the provisioning secrets.
    json_post "/auth/otp-setup", { password: PASSWORD }
    assert_response :unprocessable_entity
    secret = response.parsed_body["otp_secret"]
    raw_secret = response.parsed_body["otp_raw_secret"]
    assert secret.present?

    totp = ROTP::TOTP.new(secret)
    json_post "/auth/otp-setup",
              { password: PASSWORD, otp_secret: secret, otp_raw_secret: raw_secret, otp: totp.now }
    assert_response :success

    # Rodauth allows one OTP use per 30s window; the setup consumed the
    # current one, so shift last_use back for the login below.
    ActiveRecord::Base.connection.execute("UPDATE user_otp_keys SET last_use = last_use - interval '60 seconds'")

    # Fresh login now requires the second factor before the engine session.
    json_post "/auth/logout", {}
    json_post "/auth/login", { email: EMAIL, password: PASSWORD }
    assert_response :success
    assert_equal true, response.parsed_body["two_factor_required"]
    assert cookies["scrapix_session"].blank?, "engine session must wait for the second factor"

    get "/auth/me"
    assert_response :unauthorized

    json_post "/auth/otp-auth", { otp: totp.now }
    assert_response :success
    assert cookies["scrapix_session"].present?
    get "/auth/me"
    assert_response :success
  end
end
