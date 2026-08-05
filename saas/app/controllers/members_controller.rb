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
        joined_at: rfc3339_auto(joined_at) }
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
    token_hash = Digest::SHA256.hexdigest(raw_token)

    row = ActiveRecord::Base.connection.select_one(
      ActiveRecord::Base.sanitize_sql_array([ <<~SQL, account_id, email, role, @authenticated_user_id, token_hash ])
        INSERT INTO account_invites (account_id, email, role, invited_by, token_hash)
        VALUES (?, ?, ?, ?, ?)
        ON CONFLICT (account_id, email) WHERE status = 'pending'
        DO UPDATE SET role = EXCLUDED.role, token_hash = EXCLUDED.token_hash,
            expires_at = now() + interval '7 days', invited_by = EXCLUDED.invited_by
        RETURNING id, email, role, status, expires_at, created_at
      SQL
    )
    api_error!("Failed to create invite", "internal_error") unless row

    account_name = Account.where(id: account_id).pick(:name) || "Scrapix"
    EmailQueue.enqueue("team_invite", email, {
      account_name: account_name,
      inviter_name: @authenticated_email,
      role: role,
      token: raw_token
    })

    render json: {
      id: row["id"],
      email: row["email"],
      role: row["role"],
      status: row["status"],
      invited_by: @authenticated_user_id,
      expires_at: rfc3339_auto(row["expires_at"]),
      created_at: rfc3339_auto(row["created_at"])
    }
  end

  def update_role
    account_id = current_account_id!
    require_role!(current_role!(account_id), %w[owner])

    target = params[:user_id]
    api_error!("Invalid user ID", "validation_error") unless uuid?(target)
    api_error!("Invalid role", "validation_error") unless ALL_ROLES.include?(params[:role].to_s)
    api_error!("Cannot change your own role", "validation_error") if target == @authenticated_user_id

    updated = AccountMember.where(user_id: target, account_id: account_id)
                           .update_all(role: params[:role])
    api_error!("Member not found", "not_found") if updated.zero?

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
      EmailQueue.enqueue("member_removed", removed_email, {
        account_name: account_name,
        removed_by: @authenticated_email
      })
    end

    render json: { message: "Member removed" }
  end
end
