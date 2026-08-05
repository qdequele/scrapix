Rails.application.routes.draw do
  # Rails' built-in liveness probe (no DB check).
  get "up" => "rails/health#show", as: :rails_health_check

  # SaaS health: liveness + shared-Postgres reachability.
  get "health" => "health#show"

  # Routes migrate here from the Rust API phase by phase (SCR-85):
  # analytics pipes (phase 3, done) → configs/engines → auth/sessions →
  # account/team/keys → billing/Stripe → OAuth provider + MCP.

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
