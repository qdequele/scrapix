# GET /internal/accounts/:id — tier and balance for engine service calls
# (Rails cron) and balance refreshes.
module Internal
  class AccountsController < ApplicationController
    include ServiceAuthentication

    def show
      id = params[:id].to_s
      account = id.match?(CredentialResolver::UUID) ? Account.find_by(id: id) : nil
      return render(json: { active: false }) unless account&.active

      render json: { active: true, account_id: account.id, tier: account.tier,
                     credits: { balance: account.effective_credits_balance },
                     cache_ttl: IntrospectionsController::CACHE_TTL }
    end
  end
end
