# Processes rows of lab_events_received (debits, emails). Filled in by the
# next task; the events endpoint already enqueues it.
class ProcessLabEventsJob < ApplicationJob
  def perform; end
end
