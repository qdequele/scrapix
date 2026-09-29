# Processes pending rows of lab_events_received (debits, auto top-up, emails).
# Enqueued by POST /internal/events and swept every 10 seconds (config/
# recurring.yml) so failed events are retried after their backoff.
class ProcessLabEventsJob < ApplicationJob
  queue_as :default
  BATCH = 200

  def perform
    LabEventReceived.pending.order(:occurred_at).limit(BATCH).each { |ev| LabEvents::Processor.call(ev) }
  end
end
