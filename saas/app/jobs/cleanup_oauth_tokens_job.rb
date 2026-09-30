# Hourly sweep of expired OAuth rows (moved from the Rust engine).
class CleanupOauthTokensJob < ApplicationJob
  queue_as :default

  def perform
    OauthAuthorizationCode.where("expires_at < ?", 1.hour.ago).delete_all
    OauthToken.where("expires_at < ?", 7.days.ago)
              .or(OauthToken.where(revoked: true).where("created_at < ?", 7.days.ago))
              .delete_all
  end
end
