# Stripe billing plumbing — port of bins/scrapix-api/src/stripe.rs helpers
# and crates/scrapix-billing (pricing + idempotent credit grant).
#
# Stripe is a pure backend payment engine here: customers are created lazily,
# purchases go through real Invoices (finalize + pay) so customers get PDFs,
# and credits are granted idempotently keyed on the payment intent id.
#
# The engine-side auto-topup (charge_auto_topup, triggered by usage debits)
# intentionally stays in Rust — it belongs to the data plane.
module StripeBilling
  class StripeUnavailable < StandardError; end

  def self.configured?
    ENV["STRIPE_SECRET_KEY"].present?
  end

  # Pinned pre-Basil API version: 2025-03-31 removed `invoice.payment_intent`,
  # which the purchase flow (and the Rust implementation via async-stripe's
  # pinned version) relies on to check payment status after paying an invoice.
  # Bumping this requires migrating to the invoice `payments` list API.
  STRIPE_API_VERSION = "2024-06-20".freeze

  def self.client
    @client ||= Stripe::StripeClient.new(
      ENV.fetch("STRIPE_SECRET_KEY"),
      stripe_version: STRIPE_API_VERSION
    )
  end

  # Volume-based tiered pricing, integer math identical to
  # scrapix_billing::calculate_price_cents (ceiling of credits * rate / 10).
  def self.calculate_price_cents(credits)
    rate_tenths =
      if credits >= 10_000 then 5
      elsif credits >= 5_000 then 7
      elsif credits >= 1_000 then 8
      else 10
      end
    (credits * rate_tenths + 9) / 10
  end

  PRICING_TIERS = [
    { up_to: 999, unit_price_cents: 1.0, per_1k: 10.0 },
    { up_to: 4_999, unit_price_cents: 0.8, per_1k: 8.0 },
    { up_to: 9_999, unit_price_cents: 0.7, per_1k: 7.0 },
    { up_to: nil, unit_price_cents: 0.5, per_1k: 5.0 }
  ].freeze

  # Returns the account's Stripe customer id, creating the customer on first
  # use (name/email from the first membership, account id in metadata).
  def self.get_or_create_customer(account_id)
    existing = Account.where(id: account_id).pick(:stripe_customer_id)
    return existing if existing.present?

    row = AccountMember.where(account_id: account_id).joins(:account, :user)
                       .pick("accounts.name", "users.email")
    raise ApiErrorRendering::ApiError.new("Account not found", "not_found") unless row

    name, email = row
    customer = client.v1.customers.create(
      name: name, email: email,
      metadata: { scrapix_account_id: account_id }
    )
    Account.find(account_id).update!(stripe_customer_id: customer.id)
    customer.id
  end

  # Invoice item -> draft invoice -> finalize -> pay (expanding the payment
  # intent so the caller can check its status). Same flow as the Rust
  # create_and_pay_invoice.
  def self.create_and_pay_invoice(customer_id:, account_id:, payment_method_id:, credits:, amount_cents:, purchase_type:)
    description = "Scrapix: #{credits} credits"
    client.v1.invoice_items.create(
      customer: customer_id, amount: amount_cents, currency: "usd",
      description: description
    )

    invoice = client.v1.invoices.create(
      customer: customer_id,
      collection_method: "charge_automatically",
      auto_advance: false,
      default_payment_method: payment_method_id,
      description: description,
      pending_invoice_items_behavior: "include",
      metadata: {
        scrapix_account_id: account_id,
        credits: credits.to_s,
        type: purchase_type
      }
    )

    invoice = client.v1.invoices.finalize_invoice(invoice.id, { auto_advance: false })
    # With collection_method=charge_automatically and a default payment method,
    # finalize can charge immediately; the explicit pay then 400s with
    # "Invoice is already paid" even though the customer WAS charged. Treat
    # that as success and re-fetch to continue the ledger-crediting flow.
    # (Ports the fix/stripe-invoice-already-paid branch from the Rust API.)
    client.v1.invoices.pay(invoice.id, { expand: [ "payment_intent" ] })
  rescue Stripe::InvalidRequestError => e
    raise unless e.message.include?("already paid")

    client.v1.invoices.retrieve(invoice.id, { expand: [ "payment_intent" ] })
  end

  # Idempotent credit grant keyed on the Stripe payment intent id — mirrors
  # scrapix_billing::add_credits_for_payment (skips when a transaction with
  # this stripe_payment_intent_id already exists).
  def self.add_credits_for_payment(account_id, credits, payment_intent_id, description)
    exists = Transaction.where(account_id: account_id)
                        .where("metadata->>'stripe_payment_intent_id' = ?", payment_intent_id)
                        .exists?
    if exists
      Rails.logger.info("Payment #{payment_intent_id} already processed, skipping")
      return
    end

    Account.find(account_id).credit!(
      credits, type: "manual_topup", description: description,
      metadata: { stripe_payment_intent_id: payment_intent_id }
    )
  end

  # First member's email for the account (payment receipts) — mirrors
  # crate::email::get_account_email.
  def self.account_email(account_id)
    AccountMember.where(account_id: account_id).joins(:user).order(joined_at: :asc)
                 .pick("users.email")
  end
end
