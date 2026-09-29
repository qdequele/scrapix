# Delayed-email queue. Rails inserts rows (including the ones triggered by
# engine events: job completed/failed, auto-topup outcomes, low balance —
# see LabEvents::Processor); DrainScheduledEmailsJob delivers them via
# ActionMailer.
class ScheduledEmail < ApplicationRecord
  MAX_ATTEMPTS = 5

  scope :due, -> {
    where(sent: false)
      .where(attempts: ...MAX_ATTEMPTS)
      .where("next_attempt_at IS NULL OR next_attempt_at <= now()")
      .where(send_at: ..Time.current)
  }
end
