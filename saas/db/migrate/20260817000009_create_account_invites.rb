class CreateAccountInvites < ActiveRecord::Migration[8.1]
  def change
    create_table :account_invites, id: :uuid do |t|
      t.references :account, type: :uuid, null: false, foreign_key: { on_delete: :cascade }
      t.text :email, null: false, index: true
      t.text :role, null: false, default: "member"
      t.uuid :invited_by, null: false
      t.text :token_hash, null: false
      t.text :status, null: false, default: "pending"
      t.timestamptz :expires_at, null: false, default: -> { "now() + interval '7 days'" }
      t.timestamptz :created_at, null: false, default: -> { "now()" }

      t.check_constraint "role IN ('admin', 'member', 'viewer')", name: "account_invites_role_check"
      t.check_constraint "status IN ('pending', 'accepted', 'expired', 'revoked')", name: "account_invites_status_check"

      t.index :token_hash, where: "status = 'pending'", name: "index_account_invites_on_token_hash_pending"
      # One live invite per (account, email); the invite flow upserts on this.
      t.index [ :account_id, :email ], unique: true, where: "status = 'pending'",
              name: "index_account_invites_on_account_id_and_email_pending"
    end

    add_foreign_key :account_invites, :users, column: :invited_by, on_delete: :cascade
  end
end
