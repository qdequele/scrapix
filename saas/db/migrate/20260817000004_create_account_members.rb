class CreateAccountMembers < ActiveRecord::Migration[8.1]
  def change
    create_table :account_members, primary_key: [ :user_id, :account_id ] do |t|
      t.uuid :user_id, null: false
      t.uuid :account_id, null: false
      t.text :role, null: false, default: "owner"
      t.timestamptz :joined_at, null: false, default: -> { "now()" }

      t.check_constraint "role IN ('owner', 'admin', 'member', 'viewer')", name: "account_members_role_check"
    end

    add_foreign_key :account_members, :users, on_delete: :cascade
    add_foreign_key :account_members, :accounts, on_delete: :cascade
    add_index :account_members, :user_id
    add_index :account_members, :account_id
  end
end
