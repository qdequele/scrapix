# Engine-owned outbox: written and read only by the Rust engine (it delivers
# the rows to POST /internal/events). Rails never reads it; the DDL lives here
# because Rails owns the hosted schema (same arrangement as `jobs`).
class CreateLabEvents < ActiveRecord::Migration[8.1]
  def change
    create_table :lab_events, id: :uuid, default: nil do |t|
      t.text :type, null: false
      t.uuid :account_id, null: false
      t.jsonb :payload, null: false
      t.timestamptz :created_at, null: false, default: -> { "now()" }
      t.integer :attempts, null: false, default: 0
      t.timestamptz :next_attempt_at, null: false, default: -> { "now()" }
      t.timestamptz :delivered_at
    end
    add_index :lab_events, :next_attempt_at, where: "delivered_at IS NULL", name: "lab_events_due_idx"
    add_index :lab_events, :delivered_at, where: "delivered_at IS NOT NULL", name: "lab_events_delivered_idx"
  end
end
