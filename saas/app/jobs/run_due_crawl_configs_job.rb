# Fires saved configs whose cron schedule is due (moved from the Rust engine,
# spec 2a). One config per transaction, locked FOR UPDATE SKIP LOCKED, so
# overlapping runs (several Puma/SolidQueue processes) never fire it twice.
class RunDueCrawlConfigsJob < ApplicationJob
  queue_as :default
  # One run at a time (like ProcessLabEventsJob): the row lock already keeps
  # a config from firing twice, this just stops overlapping sweeps piling up.
  limits_concurrency to: 1, key: "run_due_crawl_configs", duration: 5.minutes, on_conflict: :discard
  MAX_PER_RUN = 50
  # Shown to the account for errors that aren't the engine's own answer;
  # the real error (hosts, internals) goes to the log only.
  UNREACHABLE = "The crawl engine could not be reached; the run will retry at the next schedule".freeze

  def perform
    return unless ENV["LAB_CRON_ENABLED"] == "true"
    MAX_PER_RUN.times { break unless fire_next }
  end

  private

  def fire_next
    CrawlConfig.transaction do
      cfg = CrawlConfig.where(cron_enabled: true).where.not(cron_expression: nil)
                       .where("next_run_at <= now()").order(:next_run_at)
                       .lock("FOR UPDATE SKIP LOCKED").first
      return false unless cfg
      fire(cfg)
      true
    end
  end

  def fire(cfg)
    next_run = CrawlConfig.next_run_for(cfg.cron_expression)
    return cfg.update_columns(cron_enabled: false, last_error: "Invalid cron expression") unless next_run
    response = ScrapixEngine.create_crawl(cfg.config, { service_account_id: cfg.account_id })
    cfg.update_columns(last_run_at: Time.current, last_job_id: response["job_id"], last_error: nil, next_run_at: next_run)
  rescue ScrapixEngine::EngineError => e
    message = (e.body.is_a?(Hash) && e.body["error"]) || "Engine error #{e.status}"
    invalid = e.status == 400 && e.body.is_a?(Hash) && e.body["code"] == "validation_error"
    cfg.update_columns(last_error: message, next_run_at: next_run, cron_enabled: invalid ? false : cfg.cron_enabled)
  rescue StandardError => e
    Rails.logger.error("Scheduled run of crawl config #{cfg.id} failed: #{e.class}: #{e.message}")
    cfg.update_columns(last_error: UNREACHABLE, next_run_at: next_run)
  end
end
