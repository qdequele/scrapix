# Drains the shared `scheduled_emails` queue (rows inserted by the Rust
# engine) and delivers via ActionMailer. Runs every 30 seconds (see
# config/recurring.yml). Failed deliveries retry with exponential backoff
# (30s, 60s, 120s, ... — up to ScheduledEmail::MAX_ATTEMPTS).
class DrainScheduledEmailsJob < ApplicationJob
  queue_as :default

  MAILERS = {
    "welcome" => ->(row) { AuthMailer.with(to: row.recipient, name: row.payload["name"]).welcome },
    "verification" => ->(row) { AuthMailer.with(to: row.recipient, name: row.payload["name"], token: row.payload.fetch("token")).verification },
    "password_reset" => ->(row) { AuthMailer.with(to: row.recipient, token: row.payload.fetch("token")).password_reset },
    "password_changed" => ->(row) { AuthMailer.with(to: row.recipient).password_changed },
    "team_invite" => ->(row) { TeamMailer.with(to: row.recipient, account_name: row.payload["account_name"], inviter_name: row.payload["inviter_name"], role: row.payload.fetch("role", "member"), token: row.payload.fetch("token")).invite },
    "invite_accepted" => ->(row) { TeamMailer.with(to: row.recipient, member_name: row.payload["member_name"], account_name: row.payload["account_name"], role: row.payload.fetch("role", "member")).invite_accepted },
    "member_removed" => ->(row) { TeamMailer.with(to: row.recipient, account_name: row.payload["account_name"], removed_by: row.payload["removed_by"]).member_removed },
    "payment_receipt" => ->(row) { BillingMailer.with(to: row.recipient, credits: row.payload["credits"].to_i, amount_cents: row.payload["amount_cents"].to_i).payment_receipt },
    "auto_topup_receipt" => ->(row) { BillingMailer.with(to: row.recipient, credits: row.payload["credits"].to_i, amount_cents: row.payload["amount_cents"].to_i, new_balance: row.payload["new_balance"].to_i).auto_topup_receipt },
    "auto_topup_failed" => ->(row) { BillingMailer.with(to: row.recipient, reason: row.payload.fetch("reason", "Unknown error")).auto_topup_failed },
    "low_balance" => ->(row) { BillingMailer.with(to: row.recipient, balance: row.payload["balance"].to_i).low_balance },
    "job_completed" => ->(row) { JobsMailer.with(to: row.recipient, job_id: row.payload["job_id"], index_uid: row.payload["index_uid"], pages_crawled: row.payload["pages_crawled"].to_i, documents_indexed: row.payload["documents_indexed"].to_i, duration_secs: row.payload["duration_secs"].to_i).completed },
    "job_failed" => ->(row) { JobsMailer.with(to: row.recipient, job_id: row.payload["job_id"], error_message: row.payload.fetch("error_message", "Unknown error"), pages_crawled: row.payload["pages_crawled"].to_i).failed }
  }.freeze

  def perform
    ScheduledEmail.due.order(:send_at).limit(50).each do |row|
      row.increment!(:attempts)
      deliver(row)
      row.update!(sent: true)
    rescue StandardError => e
      backoff = 30 * (1 << [ row.attempts, 5 ].min)
      Rails.logger.warn("Scheduled email #{row.id} (#{row.email_type}) failed: #{e.message}; retry in #{backoff}s")
      row.update!(next_attempt_at: backoff.seconds.from_now, last_error: e.message)
    end
  end

  private

  def deliver(row)
    builder = MAILERS.fetch(row.email_type) do
      raise ArgumentError, "Unknown scheduled email type: #{row.email_type}"
    end
    builder.call(row).deliver_now
  end
end
