# Profile and account-membership endpoints under /auth.
#
# The authentication flows themselves (signup, login, logout, verify-email,
# forgot/reset password, 2FA, passkeys, social login) are Rodauth routes —
# see app/misc/rodauth_main.rb. What remains here is app data: the current
# user, their accounts, and invite acceptance. Sessions are the scrapix_session
# HS256 JWT that Rodauth issues on login and the Rust engine also validates.
class AuthController < ApplicationController
  before_action :authenticate_session!

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
    invite = AccountInvite.live
                          .find_by(token_hash: Digest::SHA256.hexdigest(params[:token].to_s))
    unless invite
      return render_auth_error(:bad_request, "Invalid or expired invite token", "invalid_token")
    end
    if @authenticated_email.to_s.downcase != invite.email.downcase
      return render_auth_error(:forbidden, "This invite was sent to a different email address", "email_mismatch")
    end

    if AccountMember.exists?(user_id: @authenticated_user_id, account_id: invite.account_id)
      invite.update!(status: "accepted")
      return render json: { message: "You are already a member of this account" }
    end

    ActiveRecord::Base.transaction do
      AccountMember.create!(user_id: @authenticated_user_id, account_id: invite.account_id, role: invite.role)
      invite.update!(status: "accepted")
    end
    render json: { message: "You have joined the account as #{invite.role}" }
  end

  private

  def render_auth_error(status, message, code)
    render json: { error: message, code: code }, status: status
  end

  def user_response(user, account)
    {
      id: user.id,
      email: user.email,
      full_name: user.full_name,
      email_verified: user.verified?,
      notify_job_emails: user.notify_job_emails,
      account: account
    }
  end

  # First membership (or the selected account when X-Account-Id is set).
  def account_row_for(user_id, selected_account_id)
    scope = AccountMember.where(user_id: user_id).joins(:account)
    scope = selected_account_id ? scope.where(account_id: selected_account_id) : scope.limit(1)
    row = scope.pick("accounts.id", "accounts.name", "accounts.tier",
                     "accounts.active", "accounts.credits_balance", :role)
    return nil unless row

    id, name, tier, active, credits, role = row
    { id: id, name: name, tier: tier, active: active, role: role, credits_balance: credits }
  end
end
