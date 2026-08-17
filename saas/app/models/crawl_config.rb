class CrawlConfig < ApplicationRecord
  # API representation (contracts/src/shapes.ts SAVED_CONFIG).
  def as_json(*)
    {
      id: id,
      account_id: account_id,
      name: name,
      description: description,
      config: config,
      cron_expression: cron_expression,
      cron_enabled: cron_enabled,
      last_run_at: last_run_at&.utc&.iso8601(3),
      next_run_at: next_run_at&.utc&.iso8601(3),
      last_job_id: last_job_id,
      created_at: created_at.utc.iso8601(3),
      updated_at: updated_at.utc.iso8601(3)
    }
  end

  belongs_to :account

  validates :name, presence: true, uniqueness: { scope: :account_id }
  validates :config, presence: true
end
