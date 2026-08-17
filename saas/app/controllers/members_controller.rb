# Team member management under /account/members — port of
# bins/scrapix-api/src/auth/handlers/team.rs (list, invite, role change,
# removal). Session auth only.
class MembersController < ApplicationController
  include TeamContext
  before_action :authenticate_session!

  INVITE_ROLES = %w[admin member viewer].freeze
  ALL_ROLES = %w[owner admin member viewer].freeze

  def index
    account_id = current_account_id!
    rows = AccountMember.where(account_id: account_id)
                        .joins(:user).order(joined_at: :asc)
                        .pluck("users.id", "users.email", "users.full_name", :role, :joined_at)
    render json: rows.map { |user_id, email, full_name, role, joined_at|
      { user_id: user_id, email: email, full_name: full_name, role: role,
        joined_at: joined_at.utc.iso8601(3) }
    }
  end

  def invite
    account_id = current_account_id!
    caller_role = current_role!(account_id)
    require_role!(caller_role, %w[owner admin])

    role = params[:role].presence || "member"
    unless INVITE_ROLES.include?(role)
      api_error!("Invalid role. Must be admin, member, or viewer", "validation_error")
    end
    if caller_role == "admin" && role == "admin"
      api_error!("Admins cannot invite other admins", "forbidden")
    end

    email = params[:email].to_s.strip
    api_error!("Valid email is required", "validation_error") if email.empty? || !email.include?("@")

    already_member = AccountMember.where(account_id: account_id)
                                  .joins(:user).exists?(users: { email: email })
    api_error!("User is already a member of this account", "already_member") if already_member

    raw_token = SecureRandom.alphanumeric(48)
    invite = AccountInvite.issue!(
      account_id: account_id, email: email, role: role,
      invited_by: @authenticated_user_id,
      token_hash: Digest::SHA256.hexdigest(raw_token)
    )

    account_name = Account.where(id: account_id).pick(:name) || "Scrapix"
    TeamMailer.with(
      to: email, account_name: account_name, inviter_name: @authenticated_email,
      role: role, token: raw_token
    ).invite.deliver_later

    render json: invite
  end

  def update_role
    account_id = current_account_id!
    require_role!(current_role!(account_id), %w[owner])

    target = params[:user_id]
    api_error!("Invalid user ID", "validation_error") unless uuid?(target)
    api_error!("Invalid role", "validation_error") unless ALL_ROLES.include?(params[:role].to_s)
    api_error!("Cannot change your own role", "validation_error") if target == @authenticated_user_id

    member = AccountMember.find_by(user_id: target, account_id: account_id)
    api_error!("Member not found", "not_found") unless member
    member.update!(role: params[:role])

    render json: { message: "Role updated to #{params[:role]}" }
  end

  def remove
    account_id = current_account_id!
    target = params[:user_id]
    api_error!("Invalid user ID", "validation_error") unless uuid?(target)

    is_self = target == @authenticated_user_id
    if is_self
      if current_role!(account_id) == "owner" &&
         AccountMember.where(account_id: account_id, role: "owner").count <= 1
        api_error!("Cannot leave: you are the only owner. Transfer ownership first.", "last_owner")
      end
    else
      require_role!(current_role!(account_id), %w[owner])
    end

    removed_email = User.where(id: target).pick(:email)
    deleted = AccountMember.where(user_id: target, account_id: account_id).delete_all
    api_error!("Member not found", "not_found") if deleted.zero?

    if !is_self && removed_email
      account_name = Account.where(id: account_id).pick(:name) || "Scrapix"
      TeamMailer.with(
        to: removed_email, account_name: account_name, removed_by: @authenticated_email
      ).member_removed.deliver_later
    end

    render json: { message: "Member removed" }
  end
end
