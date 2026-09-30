class LabEventReceived < ApplicationRecord
  self.table_name = "lab_events_received"
  self.inheritance_column = nil
  # This table has no created_at (ApplicationRecord's implicit order column).
  self.implicit_order_column = "received_at"

  scope :pending, -> { where(processed_at: nil).where("next_attempt_at IS NULL OR next_attempt_at <= now()") }

  def data = payload.fetch("data", {})
end
