class ApplicationJob < ActiveJob::Base
  # Retry on deadlocks, don't crash on records deleted mid-flight.
  retry_on ActiveRecord::Deadlocked
  discard_on ActiveJob::DeserializationError
end
