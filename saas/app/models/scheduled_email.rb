# Shared delayed-email queue. The Rust engine inserts rows for the events it
# owns (job completed/failed, auto-topup outcomes, low balance);
# DrainScheduledEmailsJob delivers them via ActionMailer.
class ScheduledEmail < ApplicationRecord
  MAX_ATTEMPTS = 5

  scope :due, -> {
    where(sent: false)
      .where(attempts: ...MAX_ATTEMPTS)
      .where("next_attempt_at IS NULL OR next_attempt_at <= now()")
      .where(send_at: ..Time.current)
  }
end
