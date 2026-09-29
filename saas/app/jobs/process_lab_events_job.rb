# Processes pending rows of lab_events_received (debits, auto top-up, emails).
# Enqueued by POST /internal/events and swept every 10 seconds (config/
# recurring.yml) so failed events are retried after their backoff.
#
# One run at a time: overlapping runs (the per-request enqueue and the sweep)
# would otherwise pick the same batch and contend on the same account locks.
# A run enqueued while another holds the semaphore is discarded — the sweep
# picks up whatever it would have processed within 10 seconds.
class ProcessLabEventsJob < ApplicationJob
  queue_as :default
  limits_concurrency to: 1, key: "process_lab_events", duration: 15.minutes, on_conflict: :discard
  BATCH = 200

  def perform
    LabEventReceived.pending.order(:occurred_at).limit(BATCH).each { |ev| LabEvents::Processor.call(ev) }
  end
end
