# Auto top-up after a usage debit (moved from the Rust engine, spec 2a).
# Credits are granted only after a succeeded Stripe payment — never for free.
#
# At most one charge attempt per account is in flight or recently failed:
# under the account row lock, `claim` re-checks the balance and the cooldown
# and records the attempt in accounts.last_auto_topup_attempt_at, and that
# claim commits *before* Stripe is called. So a concurrent debit sees the
# attempt and skips, and an error or crash anywhere after the claim (even
# after a succeeded charge) can't lead a retry to charge again within
# RETRY_AFTER. A succeeded top-up clears the claim, so it never blocks the
# next one. (A charge that succeeded but wasn't credited here is credited by
# the invoice.paid webhook, idempotently on the payment intent.)
class AutoTopup
  FAILURE_EMAIL_EVERY = 24.hours
  RETRY_AFTER = 1.hour

  # => :charged | :skipped | :failed
  def self.call(account)
    claimed = claim(account)
    return claimed unless claimed == :claimed

    charge(account)
  end

  def self.claim(account)
    account.with_lock do # reloads the row FOR UPDATE
      return :skipped unless account.auto_topup_enabled && account.credits_balance < account.auto_topup_threshold
      return :skipped if account.last_auto_topup_attempt_at&.after?(RETRY_AFTER.ago)
      return fail!(account, "Monthly spend limit reached") if account.spend_limit_exceeded?(account.auto_topup_amount)
      if !StripeBilling.configured? || account.stripe_customer_id.blank? || account.stripe_default_payment_method_id.blank?
        return fail!(account, "No payment method on file")
      end

      account.update_columns(last_auto_topup_attempt_at: Time.current)
      :claimed
    end
  end

  def self.charge(account)
    amount = account.auto_topup_amount
    cents = StripeBilling.calculate_price_cents(amount)
    invoice = StripeBilling.create_and_pay_invoice(customer_id: account.stripe_customer_id, account_id: account.id,
                payment_method_id: account.stripe_default_payment_method_id, credits: amount,
                amount_cents: cents, purchase_type: "auto_topup", void_unpaid: true)
    pi = invoice.payment_intent
    case pi&.status
    when "succeeded"
      StripeBilling.add_credits_for_payment(account.id, amount, pi.id, "Auto top-up (Stripe)")
      account.update_columns(last_auto_topup_attempt_at: nil)
      if (to = account.owner_email)
        ScheduledEmail.create!(email_type: "auto_topup_receipt", recipient: to,
          payload: { credits: amount, amount_cents: cents, new_balance: account.reload.credits_balance }, send_at: Time.current)
      end
      :charged
    when "processing"
      # May still succeed (the invoice.paid webhook then grants the credits):
      # don't void it or tell the owner it failed. The claim holds off retries.
      Rails.logger.info("Auto top-up for account #{account.id} is processing (invoice #{invoice.id})")
      :failed
    else
      StripeBilling.void_invoice(invoice.id)
      fail!(account, "Payment #{pi&.status || 'failed'}")
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
  private_class_method :claim, :charge, :fail!
end
