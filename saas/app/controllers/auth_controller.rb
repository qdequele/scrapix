# Password authentication and account/session endpoints — port of
# bins/scrapix-api/src/auth/handlers/{auth,account,team}.rs with the frozen
# contract: same routes, bodies, error codes, and the HS256 session JWT the
# Rust engine keeps validating.
class AuthController < ApplicationController
  SESSION_ONLY = %i[me update_me my_accounts create_account accept_invite resend_verification].freeze
  before_action :authenticate_session!, only: SESSION_ONLY

  TOKEN_CHARS = ("A".."Z").to_a + ("a".."z").to_a + ("0".."9").to_a

  def signup
    email = params[:email].to_s
    password = params[:password].to_s
    if email.empty? || password.length < 12
      return render_auth_error(:bad_request, "Email required and password must be at least 12 characters", "validation_error")
    end
    if User.exists?(email: email)
      return render_auth_error(:conflict, "Email already registered", "email_taken")
    end

    full_name = params[:full_name].presence
    verification_token = random_token(48)
    account_name = "#{full_name || email}'s Account"

    user = account = nil
    ActiveRecord::Base.transaction do
      user = User.create!(
        email: email,
        password_hash: Argon2::Password.create(password),
        full_name: full_name,
        email_verification_token: verification_token
      )
      account = Account.create!(name: account_name)
      AccountMember.create!(user_id: user.id, account_id: account.id, role: "owner")
      Transaction.create!(
        account_id: account.id, type: "initial_deposit", amount: 100,
        balance_after: 100, description: "Welcome credit deposit"
      )
    end

    auto_accept_pending_invites(user)
    AuthMailer.with(to: email, name: full_name, token: verification_token)
              .verification.deliver_later
    set_session_cookie(user.id, email)

    render json: {
      id: user.id,
      email: email,
      full_name: full_name,
      email_verified: false,
      notify_job_emails: true,
      account: {
        id: account.id, name: account_name, tier: "free",
        active: true, role: "owner", credits_balance: 100
      }
    }
  end

  def login
    user = User.find_by(email: params[:email].to_s)
    valid = user&.password_hash.present? &&
            (Argon2::Password.verify_password(params[:password].to_s, user.password_hash) rescue false)
    unless valid
      return render_auth_error(:unauthorized, "Invalid email or password", "invalid_credentials")
    end

    set_session_cookie(user.id, user.email)
    render json: user_response(user, account_row_for(user.id, nil))
  end

  def logout
    cookies["scrapix_session"] = SessionToken.clear_cookie
    render json: { message: "Logged out" }
  end

  def verify_email
    user = User.find_by(email_verification_token: params[:token].to_s, email_verified: false)
    unless params[:token].present? && user
      return render_auth_error(:bad_request, "Invalid or expired verification token", "invalid_token")
    end

    user.update_columns(email_verified: true, email_verification_token: nil)
    AuthMailer.with(to: user.email, name: user.full_name)
              .welcome.deliver_later(wait: 120.seconds)
    render json: { message: "Email verified successfully" }
  end

  def resend_verification
    user = User.find_by(id: @authenticated_user_id)
    return render_auth_error(:not_found, "User not found", "not_found") unless user
    if user.email_verified
      return render_auth_error(:bad_request, "Email already verified", "already_verified")
    end

    token = random_token(48)
    user.update_columns(email_verification_token: token)
    AuthMailer.with(to: user.email, name: user.full_name, token: token)
              .verification.deliver_later
    render json: { message: "Verification email sent" }
  end

  def forgot_password
    generic = { message: "If an account with that email exists, we sent a password reset link." }
    user = User.find_by(email: params[:email].to_s)
    return render json: generic unless user

    raw_token = random_token(48)
    PasswordResetToken.where(user_id: user.id, used: false).update_all(used: true)
    PasswordResetToken.create!(
      user_id: user.id,
      token_hash: Digest::SHA256.hexdigest(raw_token),
      expires_at: 1.hour.from_now
    )
    AuthMailer.with(to: user.email, token: raw_token).password_reset.deliver_later
    render json: generic
  end

  def reset_password
    if params[:password].to_s.length < 12
      return render_auth_error(:bad_request, "Password must be at least 12 characters", "validation_error")
    end

    token = PasswordResetToken.usable.find_by(token_hash: Digest::SHA256.hexdigest(params[:token].to_s))
    unless token
      return render_auth_error(:bad_request, "Invalid or expired reset token", "invalid_token")
    end

    ActiveRecord::Base.transaction do
      token.update!(used: true)
      User.find(token.user_id).update!(password_hash: Argon2::Password.create(params[:password].to_s))
    end
    if (email = User.where(id: token.user_id).pick(:email))
      AuthMailer.with(to: email).password_changed.deliver_later
    end
    render json: { message: "Password reset successfully. Please log in with your new password." }
  end

  def me
    user = User.find_by(id: @authenticated_user_id)
    return render_auth_error(:not_found, "User not found", "not_found") unless user

    render json: user_response(user, account_row_for(user.id, @selected_account_id))
  end

  def update_me
    updates = {}
    updates[:full_name] = params[:full_name] if params.key?(:full_name) && params[:full_name].present?
    unless params[:notify_job_emails].nil?
      updates[:notify_job_emails] = ActiveModel::Type::Boolean.new.cast(params[:notify_job_emails])
    end
    User.find(@authenticated_user_id).update!(updates) if updates.any?
    render json: { message: "Updated" }
  end

  def my_accounts
    rows = AccountMember.where(user_id: @authenticated_user_id)
                        .joins(:account).order(joined_at: :asc)
                        .pluck("accounts.id", "accounts.name", "accounts.tier",
                               "accounts.active", "accounts.credits_balance", :role)
    render json: rows.map { |id, name, tier, active, credits, role|
      { id: id, name: name, tier: tier, active: active, role: role, credits_balance: credits }
    }
  end

  def create_account
    name = params[:name].to_s.strip
    if name.empty?
      return render_auth_error(:bad_request, "Account name is required", "validation_error")
    end

    account = nil
    ActiveRecord::Base.transaction do
      account = Account.create!(name: name)
      AccountMember.create!(user_id: @authenticated_user_id, account_id: account.id, role: "owner")
      Transaction.create!(
        account_id: account.id, type: "initial_deposit", amount: 100,
        balance_after: 100, description: "Welcome credit deposit"
      )
    end
    render json: {
      id: account.id, name: name, tier: "free", active: true,
      role: "owner", credits_balance: 100
    }, status: :created
  end

  def accept_invite
    invite = AccountInvite.pending.where("expires_at > now()")
                          .find_by(token_hash: Digest::SHA256.hexdigest(params[:token].to_s))
    unless invite
      return render_auth_error(:bad_request, "Invalid or expired invite token", "invalid_token")
    end
    if @authenticated_email.to_s.downcase != invite.email.downcase
      return render_auth_error(:forbidden, "This invite was sent to a different email address", "email_mismatch")
    end

    if AccountMember.exists?(user_id: @authenticated_user_id, account_id: invite.account_id)
      invite.update_columns(status: "accepted")
      return render json: { message: "You are already a member of this account" }
    end

    ActiveRecord::Base.transaction do
      AccountMember.create!(user_id: @authenticated_user_id, account_id: invite.account_id, role: invite.role)
      invite.update_columns(status: "accepted")
    end
    render json: { message: "You have joined the account as #{invite.role}" }
  end

  private

  def render_auth_error(status, message, code)
    render json: { error: message, code: code }, status: status
  end

  def set_session_cookie(user_id, email)
    cookies["scrapix_session"] = SessionToken.cookie(SessionToken.encode(user_id, email))
  end

  def random_token(length)
    Array.new(length) { TOKEN_CHARS.sample }.join
  end

  def user_response(user, account)
    {
      id: user.id,
      email: user.email,
      full_name: user.full_name,
      email_verified: user.email_verified,
      notify_job_emails: user.notify_job_emails,
      account: account
    }
  end

  # First membership (or the selected account when X-Account-Id is set),
  # matching the Rust handlers' account resolution for /auth/me and login.
  def account_row_for(user_id, selected_account_id)
    scope = AccountMember.where(user_id: user_id).joins(:account)
    scope = selected_account_id ? scope.where(account_id: selected_account_id) : scope.limit(1)
    row = scope.pick("accounts.id", "accounts.name", "accounts.tier",
                     "accounts.active", "accounts.credits_balance", :role)
    return nil unless row

    id, name, tier, active, credits, role = row
    { id: id, name: name, tier: tier, active: active, role: role, credits_balance: credits }
  end

  def auto_accept_pending_invites(user)
    AccountInvite.pending.where("expires_at > now()").where(email: user.email).find_each do |invite|
      AccountMember.create_or_find_by(user_id: user.id, account_id: invite.account_id) do |m|
        m.role = invite.role
      end
      invite.update_columns(status: "accepted")
    rescue ActiveRecord::ActiveRecordError => e
      Rails.logger.warn("Failed to auto-accept invite #{invite.id}: #{e.message}")
    end
  end
end
