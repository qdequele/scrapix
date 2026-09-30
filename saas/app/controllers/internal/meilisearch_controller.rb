# GET /internal/accounts/:account_id/meilisearch — the engine's Meilisearch
# target for an account: the engine matching ?url=, else the default one.
module Internal
  class MeilisearchController < ApplicationController
    include ServiceAuthentication

    def show
      account_id = params[:account_id].to_s
      return render(json: { error: "not_found" }, status: :not_found) unless account_id.match?(CredentialResolver::UUID)

      engine = find_engine(account_id)
      return render(json: { error: "not_found" }, status: :not_found) unless engine

      render json: { id: engine.id, url: engine.url, api_key: engine.api_key.to_s }
    end

    private

    def find_engine(account_id)
      scope = MeilisearchEngine.where(account_id: account_id)
      return scope.find_by(is_default: true) if params[:url].blank?

      wanted = params[:url].to_s.chomp("/")
      scope.order(is_default: :desc).detect { |e| e.url.to_s.chomp("/") == wanted }
    end
  end
end
