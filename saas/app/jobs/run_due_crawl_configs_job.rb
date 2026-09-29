# Fires saved configs whose cron schedule is due (moved from the Rust engine,
# spec 2a). One config per transaction, locked FOR UPDATE SKIP LOCKED, so
# overlapping runs (several Puma/SolidQueue processes) never fire it twice.
class RunDueCrawlConfigsJob < ApplicationJob
  queue_as :default
  MAX_PER_RUN = 50

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
    cfg.update_columns(last_error: e.message.truncate(500), next_run_at: next_run)
  end
end
