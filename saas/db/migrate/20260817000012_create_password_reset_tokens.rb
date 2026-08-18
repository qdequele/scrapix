class CreatePasswordResetTokens < ActiveRecord::Migration[8.1]
  def change
    create_table :password_reset_tokens, id: :uuid do |t|
      t.references :user, type: :uuid, null: false, foreign_key: { on_delete: :cascade }
      t.text :token_hash, null: false
      t.timestamptz :expires_at, null: false
      t.boolean :used, null: false, default: false
      t.timestamptz :created_at, null: false, default: -> { "now()" }

      t.index :token_hash, where: "used = false", name: "index_password_reset_tokens_on_token_hash_usable"
    end
  end
end
