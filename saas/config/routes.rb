Rails.application.routes.draw do
  # Rails' built-in liveness probe (no DB check).
  get "up" => "rails/health#show", as: :rails_health_check

  # SaaS health: liveness + shared-Postgres reachability.
  get "health" => "health#show"

  # Routes migrate here from the Rust API phase by phase (SCR-85):
  # analytics pipes → configs/engines → auth/sessions → account/team/keys →
  # billing/Stripe → OAuth provider + MCP.
end
