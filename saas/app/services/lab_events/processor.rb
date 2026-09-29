module LabEvents
  # Processes one received event. Success → processed_at; a raised error →
  # attempts+1 and backoff (the recurring sweep retries it). An event that can
  # never succeed (unknown type or account, invalid data) is rejected: marked
  # processed with the reason in `error`, never retried.
  class Processor
    def self.call(event)
      case event.type
      when "usage.recorded" then UsageDebit.call(event)
      when "job.completed", "job.failed" then JobNotification.call(event)
      else return reject(event, "unknown type #{event.type}")
      end
      event.update!(processed_at: Time.current, error: nil) unless event.processed_at
    rescue StandardError => e
      attempts = event.attempts + 1
      event.update_columns(attempts: attempts, error: e.message.truncate(500),
                           next_attempt_at: (30 * (1 << [ attempts, 6 ].min)).seconds.from_now)
      Rails.logger.warn("Lab event #{event.id} (#{event.type}) failed: #{e.message}")
    end

    def self.reject(event, reason)
      Rails.logger.warn("Rejecting lab event #{event.id} (#{event.type}): #{reason}")
      event.update!(processed_at: Time.current, error: reason)
    end
  end
end
