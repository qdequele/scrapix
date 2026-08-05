# Brute-force protection on auth endpoints, matching the Rust engine's
# limiter (bins/scrapix-api/src/rate_limit.rs): 5 attempts/min/IP by default,
# AUTH_RATE_LIMIT env override for contract-test runs, Redis-backed when
# REDIS_URL is set (shared window with other instances), 429 body identical
# to the Rust response.
class Rack::Attack
  AUTH_PATHS = %w[
    /auth/signup /auth/login /auth/forgot-password /auth/reset-password
  ].freeze

  if ENV["REDIS_URL"].present?
    Rack::Attack.cache.store = ActiveSupport::Cache::RedisCacheStore.new(url: ENV["REDIS_URL"])
  end

  throttle("auth/ip", limit: proc { ENV.fetch("AUTH_RATE_LIMIT", "5").to_i }, period: 60) do |req|
    req.ip if req.post? && AUTH_PATHS.include?(req.path)
  end

  self.throttled_responder = lambda do |request|
    retry_after = (request.env["rack.attack.match_data"] || {})[:period] || 60
    [
      429,
      { "content-type" => "application/json", "retry-after" => retry_after.to_s },
      [ {
        error: "Too many authentication attempts. Please try again later.",
        code: "rate_limit_exceeded",
        retry_after_seconds: retry_after
      }.to_json ]
    ]
  end
end
