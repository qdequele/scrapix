class CreateScheduledEmails < ActiveRecord::Migration[8.1]
  def change
    # Shared delayed-email queue: both backends insert; delivery is being
    # moved from the Rust engine to Rails (SCR-87 I4).
    create_table :scheduled_emails, id: :uuid do |t|
      t.text :email_type, null: false
      t.text :recipient, null: false
      t.jsonb :payload, null: false, default: {}
      t.timestamptz :send_at, null: false
      t.boolean :sent, null: false, default: false
      t.integer :attempts, null: false, default: 0
      t.timestamptz :next_attempt_at
      t.text :last_error
      t.timestamptz :created_at, null: false, default: -> { "now()" }

      t.index :send_at, where: "sent = false", name: "index_scheduled_emails_on_send_at_pending"
    end
  end
end
