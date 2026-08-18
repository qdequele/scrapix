Rails.application.routes.draw do
  # Rails' built-in liveness probe (no DB check).
  get "up" => "rails/health#show", as: :rails_health_check

  # SaaS health: liveness + shared-Postgres reachability.
  get "health" => "health#show"

  # Routes migrate here from the Rust API phase by phase (SCR-85):
  # analytics pipes (phase 3, done) → configs/engines (phase 4, done) →
  # auth/sessions → account/team/keys → billing/Stripe → OAuth provider + MCP.

  # Authentication flows (signup/login/logout/verify/reset, TOTP 2FA,
  # WebAuthn passkeys, Google/GitHub social login) are Rodauth routes under
  # /auth, served by the Rodauth::Rails middleware (app/misc/rodauth_app.rb).
  # What remains here is profile/membership data.
  scope "auth", controller: :auth do
    get "me", action: :me
    patch "me", action: :update_me
    get "me/accounts", action: :my_accounts
    post "me/accounts", action: :create_account
    post "accept-invite", action: :accept_invite
  end

  # Phase 6: account, team, invites, API keys, non-Stripe billing.
  # (Stripe routes under /account/billing stay on the Rust engine until
  # phase 7 — the console proxy excludes them from the account prefix.)
  get "account", to: "accounts#show"
  patch "account", to: "accounts#update"
  scope "account" do
    get "members", to: "members#index"
    post "members/invite", to: "members#invite"
    patch "members/:user_id", to: "members#update_role"
    delete "members/:user_id", to: "members#remove"
    get "invites", to: "invites#index"
    delete "invites/:id", to: "invites#revoke"
    get "api-keys", to: "api_keys#index"
    post "api-keys", to: "api_keys#create"
    patch "api-keys/:id", to: "api_keys#revoke"
    get "billing", to: "billing#show"
    patch "billing", to: "billing#update"
    post "billing/topup", to: "billing#topup"
    patch "billing/auto-topup", to: "billing#auto_topup"
    patch "billing/spend-limit", to: "billing#spend_limit"
    get "billing/transactions", to: "billing#transactions"

    # Phase 7: Stripe payment routes (404 when STRIPE_SECRET_KEY is unset,
    # matching the Rust API's conditional mounting).
    post "billing/setup-intent", to: "stripe_billing#setup_intent"
    get "billing/payment-methods", to: "stripe_billing#payment_methods"
    delete "billing/payment-methods/:id", to: "stripe_billing#delete_payment_method"
    patch "billing/default-payment-method", to: "stripe_billing#set_default_payment_method"
    post "billing/purchase", to: "stripe_billing#purchase"
    get "billing/invoices", to: "stripe_billing#invoices"
    get "billing/pricing", to: "stripe_billing#pricing"
  end

  # Stripe webhook (signature-verified, no session auth).
  post "webhooks/stripe", to: "stripe_webhooks#receive"

  # Phase 8: OAuth 2.1 provider (RFC 8414/7591/7636/7009) + MCP.
  get "/.well-known/oauth-authorization-server", to: "oauth#metadata", format: false
  get "/.well-known/oauth-protected-resource", to: "oauth#protected_resource", format: false
  scope "oauth", controller: :oauth do
    post "register", action: :register
    get "authorize", action: :authorize_form
    post "authorize", action: :authorize
    post "token", action: :token
    post "revoke", action: :revoke
  end
  match "mcp", to: "mcp#handle", via: [ :get, :post, :delete ]

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
