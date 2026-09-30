# Boot-time warning for a saved-config cron that can't reach the engine.
# With LAB_CRON_ENABLED=true, RunDueCrawlConfigsJob calls the engine at
# SCRAPIX_ENGINE_URL with LAB_SERVICE_TOKEN; without either, every scheduled
# run fails (or, with the default URL, hits whatever listens on :8080).
# A warning, not a refusal: the rest of the app works without cron.
module LabCronCheck
  REQUIRED = %w[LAB_SERVICE_TOKEN SCRAPIX_ENGINE_URL].freeze

  # => warning message, or nil when cron is off or fully configured.
  def self.warning(env = ENV)
    return nil unless env["LAB_CRON_ENABLED"] == "true"

    missing = REQUIRED.select { |k| env[k].to_s.strip.empty? }
    return nil if missing.empty?

    "LAB_CRON_ENABLED=true but #{missing.join(' and ')} #{missing.one? ? 'is' : 'are'} not set: " \
      "scheduled crawls will fail" \
      "#{missing.include?('SCRAPIX_ENGINE_URL') ? ' (the engine URL defaults to http://localhost:8080)' : ''}. " \
      "See docs/operations/crawl-engine-rollout.mdx."
  end
end

Rails.application.config.after_initialize do
  message = LabCronCheck.warning
  Rails.logger.warn(message) if message
end
