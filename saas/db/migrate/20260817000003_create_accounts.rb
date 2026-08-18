class CreateAccounts < ActiveRecord::Migration[8.1]
  def change
    create_table :accounts, id: :uuid do |t|
      t.text :name, null: false
      t.text :tier, null: false, default: "free"
      t.boolean :active, null: false, default: true
      t.text :stripe_customer_id
      t.text :stripe_default_payment_method_id
      t.bigint :credits_balance, null: false, default: 100
      t.boolean :auto_topup_enabled, null: false, default: false
      t.bigint :auto_topup_amount, null: false, default: 5000
      t.bigint :auto_topup_threshold, null: false, default: 500
      t.bigint :monthly_spend_limit

      t.timestamps default: -> { "now()" }

      t.check_constraint "tier IN ('free', 'starter', 'pro', 'enterprise')", name: "accounts_tier_check"
    end
  end
end
