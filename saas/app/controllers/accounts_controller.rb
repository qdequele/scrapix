# GET/PATCH /account — port of get_account/update_account
# (bins/scrapix-api/src/auth/handlers/account.rs). Session auth only.
class AccountsController < ApplicationController
  include TeamContext
  before_action :authenticate_session!

  def show
    account_id = current_account_id!
    row = AccountMember.where(user_id: @authenticated_user_id, account_id: account_id)
                       .joins(:account)
                       .pick("accounts.id", "accounts.name", "accounts.tier",
                             "accounts.active", "accounts.credits_balance", :role)
    api_error!("Account not found", "not_found") unless row

    id, name, tier, active, credits, role = row
    render json: { id: id, name: name, tier: tier, active: active, role: role, credits_balance: credits }
  end

  def update
    account_id = current_account_id!
    require_role!(current_role!(account_id), %w[owner])

    if params[:name].present?
      Account.where(id: account_id).update_all(name: params[:name])
    end
    render json: { message: "Updated" }
  end
end
