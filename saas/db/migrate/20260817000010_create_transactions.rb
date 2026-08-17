class CreateTransactions < ActiveRecord::Migration[8.1]
  def change
    create_table :transactions, id: :uuid do |t|
      t.references :account, type: :uuid, null: false, foreign_key: { on_delete: :cascade }
      t.text :type, null: false
      t.bigint :amount, null: false
      t.bigint :balance_after, null: false
      t.text :description
      t.jsonb :metadata
      t.timestamptz :created_at, null: false, default: -> { "now()" }

      t.check_constraint "type IN ('initial_deposit', 'manual_topup', 'auto_topup', 'usage_deduction', 'refund', 'adjustment')",
                         name: "transactions_type_check"

      t.index :created_at, order: { created_at: :desc }
      t.index [ :account_id, :created_at ], order: { created_at: :desc }
    end
  end
end
