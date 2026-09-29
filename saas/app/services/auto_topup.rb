# Auto top-up after a usage debit (moved from the Rust engine, spec 2a).
# Credits are granted only after a succeeded Stripe payment — never for free.
# The Stripe charge runs under the account row lock, so concurrent debits of
# the same account can't double-charge: the next one sees the new balance.
class AutoTopup
  FAILURE_EMAIL_EVERY = 24.hours

  # => :charged | :skipped | :failed
  def self.call(account)
    account.with_lock do
      account.reload
      return :skipped unless account.auto_topup_enabled && account.credits_balance < account.auto_topup_threshold

      amount = account.auto_topup_amount
      return fail!(account, "Monthly spend limit reached") if account.spend_limit_exceeded?(amount)
      if !StripeBilling.configured? || account.stripe_customer_id.blank? || account.stripe_default_payment_method_id.blank?
        return fail!(account, "No payment method on file")
      end

      cents = StripeBilling.calculate_price_cents(amount)
      result = StripeBilling.create_and_pay_invoice(customer_id: account.stripe_customer_id, account_id: account.id,
                 payment_method_id: account.stripe_default_payment_method_id, credits: amount,
                 amount_cents: cents, purchase_type: "auto_topup")
      pi = result.payment_intent
      return fail!(account, "Payment #{pi&.status || 'failed'}") unless pi&.status == "succeeded"

      StripeBilling.add_credits_for_payment(account.id, amount, pi.id, "Auto top-up (Stripe)")
      if (to = account.owner_email)
        ScheduledEmail.create!(email_type: "auto_topup_receipt", recipient: to,
          payload: { credits: amount, amount_cents: cents, new_balance: account.reload.credits_balance }, send_at: Time.current)
      end
      :charged
    end
  rescue Stripe::StripeError => e
    fail!(account, e.message)
  end

  # Emails the owner at most once per FAILURE_EMAIL_EVERY.
  def self.fail!(account, reason)
    Rails.logger.warn("Auto top-up failed for account #{account.id}: #{reason}")
    to = account.owner_email
    recent = ScheduledEmail.where(email_type: "auto_topup_failed", recipient: to).where("created_at > ?", FAILURE_EMAIL_EVERY.ago).exists?
    ScheduledEmail.create!(email_type: "auto_topup_failed", recipient: to, payload: { reason: reason }, send_at: Time.current) if to && !recent
    :failed
  end
  private_class_method :fail!
end
