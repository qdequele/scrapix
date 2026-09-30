# Events received from product engines (Scrapix today) over POST /internal/events.
# Rails-owned. `id` is the engine's event id: inserting it once is the
# idempotency guarantee for everything the event triggers.
class CreateLabEventsReceived < ActiveRecord::Migration[8.1]
  def change
    create_table :lab_events_received, id: :uuid, default: nil do |t|
      t.text :type, null: false
      t.uuid :account_id, null: false
      t.jsonb :payload, null: false
      t.timestamptz :occurred_at, null: false
      t.timestamptz :received_at, null: false, default: -> { "now()" }
      t.timestamptz :processed_at
      t.integer :attempts, null: false, default: 0
      t.timestamptz :next_attempt_at
      t.text :error
    end
    add_index :lab_events_received, :occurred_at, where: "processed_at IS NULL", name: "lab_events_received_pending_idx"
  end
end
