Rails.application.routes.draw do
  # Rails' built-in liveness probe (no DB check).
  get "up" => "rails/health#show", as: :rails_health_check

  # SaaS health: liveness + shared-Postgres reachability.
  get "health" => "health#show"

  # Routes migrate here from the Rust API phase by phase (SCR-85):
  # analytics pipes (phase 3, done) → configs/engines (phase 4, done) →
  # auth/sessions → account/team/keys → billing/Stripe → OAuth provider + MCP.

  # Phase 5: password auth, sessions, and social login.
  scope "auth", controller: :auth do
    post "signup", action: :signup
    post "login", action: :login
    post "logout", action: :logout
    get "verify-email", action: :verify_email
    post "forgot-password", action: :forgot_password
    post "reset-password", action: :reset_password
    post "resend-verification", action: :resend_verification
    get "me", action: :me
    patch "me", action: :update_me
    get "me/accounts", action: :my_accounts
    post "me/accounts", action: :create_account
    post "accept-invite", action: :accept_invite
    get "social/:provider", to: "social_auth#initiate"
    get "social/:provider/callback", to: "social_auth#callback"
  end

  # Phase 4: saved crawl configs + Meilisearch engine registry.
  resources :configs, only: [ :create, :index, :show, :update, :destroy ] do
    post :trigger, on: :member
  end
  resources :engines, only: [ :create, :index, :show, :update, :destroy ] do
    member do
      post :default, action: :set_default
      get :indexes
      post "indexes/:index_uid/search", action: :search, constraints: { index_uid: %r{[^/]+} }
    end
  end

  # Phase 3: Tinybird-style analytics pipes (ClickHouse-backed, unauthenticated
  # to match the Rust API; 404 when ClickHouse is not configured).
  scope "analytics/v0", controller: :analytics, format: false do
    get "pipes", action: :pipes
    %w[
      top_domains domain_stats hourly_stats daily_stats error_distribution
      job_stats kpis ai_usage job_timeline job_event_summary account_usage
      account_daily_usage account_daily_usage_by_operation api_key_usage
    ].each do |pipe|
      get "pipes/#{pipe}.json", action: pipe.to_sym
    end
  end
end
