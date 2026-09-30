# One usage_deduction per received lab event (LabEvents::UsageDebit), and one
# credit grant per Stripe payment intent (StripeBilling.add_credits_for_payment).
# The payment-intent index may already exist from the free-credit hotfix.
class AddLabEventIdIndexToTransactions < ActiveRecord::Migration[8.1]
  def change
    add_index :transactions, "(metadata->>'lab_event_id')", unique: true,
              where: "metadata ? 'lab_event_id'", name: "index_transactions_on_lab_event_id"
    unless index_name_exists?(:transactions, "index_transactions_on_stripe_payment_intent_id")
      add_index :transactions, "(metadata->>'stripe_payment_intent_id')", unique: true,
                where: "metadata ? 'stripe_payment_intent_id'", name: "index_transactions_on_stripe_payment_intent_id"
    end
  end
end
