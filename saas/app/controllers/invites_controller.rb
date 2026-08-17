# Pending invite listing/revocation under /account/invites — port of
# list_invites/revoke_invite (bins/scrapix-api/src/auth/handlers/team.rs).
class InvitesController < ApplicationController
  include TeamContext
  before_action :authenticate_session!

  def index
    account_id = current_account_id!
    require_role!(current_role!(account_id), %w[owner admin])

    invites = AccountInvite.pending.where(account_id: account_id)
                           .where("expires_at > now()")
                           .order(created_at: :desc)
    render json: invites.map { |i|
      {
        id: i.id, email: i.email, role: i.role, status: i.status,
        invited_by: i.invited_by,
        expires_at: rfc3339_auto(i.expires_at),
        created_at: rfc3339_auto(i.created_at)
      }
    }
  end

  def revoke
    account_id = current_account_id!
    require_role!(current_role!(account_id), %w[owner admin])
    api_error!("Invalid invite ID", "validation_error") unless uuid?(params[:id])

    invite = AccountInvite.pending.find_by(id: params[:id], account_id: account_id)
    api_error!("Invite not found or already processed", "not_found") unless invite
    invite.update!(status: "revoked")

    render json: { message: "Invite revoked" }
  end
end
