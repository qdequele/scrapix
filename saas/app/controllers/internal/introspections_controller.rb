# POST /internal/auth/introspect — the engine resolves a client credential
# to an account (spec: Lab split section 3.1). RFC 7662 style: always 200,
# with {active: false} for anything invalid.
module Internal
  class IntrospectionsController < ApplicationController
    include ServiceAuthentication

    CACHE_TTL = 30
    KINDS = %w[api_key bearer session].freeze

    def create
      kind = params[:kind].to_s
      credential = params[:credential].to_s
      return head :bad_request unless KINDS.include?(kind) && credential.present?

      identity = resolve(kind, credential, params[:account_id].presence)
      return render(json: { active: false }) unless identity

      account = Account.find_by(id: identity[:account_id])
      return render(json: { active: false }) unless account&.active

      render json: {
        active: true, account_id: account.id, tier: account.tier, role: identity[:role],
        api_key_id: identity[:api_key_id], principal: identity[:principal],
        credits: { balance: account.effective_credits_balance }, cache_ttl: CACHE_TTL
      }
    end

    private

    def resolve(kind, credential, account_id)
      case kind
      when "api_key"
        row = CredentialResolver.api_key(credential)
        row && row[:active] && { account_id: row[:account_id], api_key_id: row[:api_key_id], role: nil,
                                 principal: { type: "api_key", user_id: nil } }
      when "bearer"
        holder = CredentialResolver.oauth(credential)
        holder && { account_id: holder[:account_id], api_key_id: nil, role: nil,
                    principal: { type: "oauth", user_id: holder[:user_id] } }
      when "session"
        user = CredentialResolver.session_user(credential)
        member = user && CredentialResolver.membership(user[:user_id], account_id)
        member && { account_id: member[:account_id], api_key_id: nil, role: member[:role],
                    principal: { type: "session", user_id: user[:user_id] } }
      end
    end
  end
end
