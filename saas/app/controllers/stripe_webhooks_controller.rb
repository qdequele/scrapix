# POST /webhooks/stripe — port of the Rust stripe_webhook handler. No auth;
# requests are authenticated by the Stripe signature. Error responses are
# plain text (matching the Rust handler), success is an empty 200.
class StripeWebhooksController < ApplicationController
  def receive
    return head :not_found unless StripeBilling.configured?

    signature = request.headers["Stripe-Signature"]
    return render plain: "Missing stripe-signature header", status: :bad_request if signature.blank?

    secret = ENV["STRIPE_WEBHOOK_SECRET"]
    if secret.blank?
      return render plain: "Webhook secret not configured", status: :internal_server_error
    end

    payload = request.body.read
    begin
      event = Stripe::Webhook.construct_event(payload, signature, secret)
    rescue Stripe::SignatureVerificationError, JSON::ParserError => e
      Rails.logger.warn("Webhook signature verification failed: #{e.message}")
      return render plain: "Webhook signature verification failed", status: :bad_request
    end

    case event.type
    when "invoice.paid"
      handle_invoice_paid(event.data.object)
    when "payment_intent.succeeded"
      handle_payment_intent_succeeded(event.data.object)
    when "payment_intent.payment_failed"
      Rails.logger.warn("Payment failed for PaymentIntent #{event.data.object.id}")
    when "setup_intent.succeeded"
      handle_setup_intent_succeeded(event.data.object)
    end

    head :ok
  end

  private

  def handle_invoice_paid(invoice)
    account_id = invoice.metadata&.[]("scrapix_account_id")
    return if account_id.blank?

    credits = Integer(invoice.metadata&.[]("credits").to_s, exception: false)
    if credits.nil?
      Rails.logger.warn("Invoice #{invoice.id} missing credits metadata")
      return
    end
    return unless valid_uuid?(account_id)

    pi_id = payment_intent_id(invoice.payment_intent) || invoice.id
    StripeBilling.add_credits_for_payment(account_id, credits, pi_id, "Credit purchase (Invoice)")
    queue_receipt(account_id, credits, invoice.amount_paid || 0)
  rescue ActiveRecord::ActiveRecordError => e
    Rails.logger.error("Failed to add credits from invoice webhook: #{e.message}")
  end

  def handle_payment_intent_succeeded(pi)
    account_id = pi.metadata&.[]("scrapix_account_id")
    if account_id.blank?
      Rails.logger.warn("PaymentIntent #{pi.id} missing scrapix_account_id metadata")
      return
    end

    credits = Integer(pi.metadata&.[]("credits").to_s, exception: false)
    if credits.nil?
      Rails.logger.warn("PaymentIntent #{pi.id} missing credits metadata")
      return
    end
    return unless valid_uuid?(account_id)

    StripeBilling.add_credits_for_payment(account_id, credits, pi.id, "Credit purchase (Stripe)")
    queue_receipt(account_id, credits, pi.amount)
  rescue ActiveRecord::ActiveRecordError => e
    Rails.logger.error("Failed to add credits from webhook: #{e.message}")
  end

  def handle_setup_intent_succeeded(si)
    account_id = si.metadata&.[]("scrapix_account_id")
    return if account_id.blank? || !valid_uuid?(account_id)

    pm_id = si.payment_method.is_a?(String) ? si.payment_method : si.payment_method&.id
    return if pm_id.blank?

    # Set as default only when the account has no default yet.
    Account.where(id: account_id, stripe_default_payment_method_id: nil)
           .update_all(stripe_default_payment_method_id: pm_id)
  end

  def payment_intent_id(payment_intent)
    return nil if payment_intent.nil?

    payment_intent.is_a?(String) ? payment_intent : payment_intent.id
  end

  def queue_receipt(account_id, credits, amount_cents)
    email = StripeBilling.account_email(account_id)
    return unless email

    BillingMailer.with(to: email, credits: credits, amount_cents: amount_cents)
                 .payment_receipt.deliver_later
  end

  def valid_uuid?(value)
    value.to_s.match?(/\A[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z/i)
  end
end
