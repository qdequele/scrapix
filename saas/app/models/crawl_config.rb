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
      last_error: last_error,
      created_at: created_at.utc.iso8601(3),
      updated_at: updated_at.utc.iso8601(3)
    }
  end

  belongs_to :account

  # Next fire time for a cron expression, or nil when it is not valid cron.
  def self.next_run_for(expression)
    cron = Fugit.parse_cron(expression.to_s)
    cron&.next_time(Time.current)&.to_t&.utc
  end

  validates :name, presence: true, uniqueness: { scope: :account_id }
  validates :config, presence: true
end
