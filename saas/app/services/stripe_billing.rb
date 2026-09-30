# Stripe billing plumbing — port of bins/scrapix-api/src/stripe.rs helpers
# and crates/scrapix-billing (pricing + idempotent credit grant).
#
# Stripe is a pure backend payment engine here: customers are created lazily,
# purchases go through real Invoices (finalize + pay) so customers get PDFs,
# and credits are granted idempotently keyed on the payment intent id.
#
# Auto top-up (triggered by usage debits the engine reports as lab events)
# lives in AutoTopup, which uses create_and_pay_invoice and
# add_credits_for_payment below.
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

  # Draft invoice -> invoice item attached to it -> finalize -> pay
  # (expanding the payment intent so the caller can check its status). The
  # invoice is created first, excluding pending items, and the item is
  # attached to it: an item created on its own would sit pending on the
  # customer if the invoice call then failed, and be swept into (and paid by)
  # the next invoice, charging twice for one credit grant. If attaching the
  # item fails, the empty draft is deleted (best effort). With void_unpaid,
  # an invoice whose /pay raises (e.g. a declined card) is voided before
  # re-raising, so unattended retries (auto top-up) don't leave open
  # invoices behind.
  def self.create_and_pay_invoice(customer_id:, account_id:, payment_method_id:, credits:, amount_cents:, purchase_type:,
                                  void_unpaid: false)
    description = "Scrapix: #{credits} credits"
    invoice = client.v1.invoices.create(
      customer: customer_id,
      currency: "usd",
      collection_method: "charge_automatically",
      auto_advance: false,
      default_payment_method: payment_method_id,
      description: description,
      pending_invoice_items_behavior: "exclude",
      metadata: {
        scrapix_account_id: account_id,
        credits: credits.to_s,
        type: purchase_type
      }
    )

    begin
      client.v1.invoice_items.create(
        customer: customer_id, invoice: invoice.id, amount: amount_cents, currency: "usd",
        description: description
      )
    rescue StandardError
      delete_draft_invoice(invoice.id)
      raise
    end

    invoice = client.v1.invoices.finalize_invoice(invoice.id, { auto_advance: false })
    begin
      client.v1.invoices.pay(invoice.id, { expand: [ "payment_intent" ] })
    rescue Stripe::StripeError => e
      # With collection_method=charge_automatically and a default payment
      # method, finalize can charge immediately; the explicit pay then 400s
      # with "Invoice is already paid" even though the customer WAS charged.
      # Treat that as success and re-fetch to continue the ledger-crediting
      # flow. (Ports the fix/stripe-invoice-already-paid branch from the Rust API.)
      if e.is_a?(Stripe::InvalidRequestError) && e.message.include?("already paid")
        return client.v1.invoices.retrieve(invoice.id, { expand: [ "payment_intent" ] })
      end

      void_invoice(invoice.id) if void_unpaid
      raise
    end
  end

  # Best-effort delete of a draft invoice (never raises; drafts can't be
  # voided). A failure is only logged: the draft holds at most its own item,
  # never a pending one, so it can't be charged by a later invoice.
  def self.delete_draft_invoice(invoice_id)
    client.v1.invoices.delete(invoice_id)
  rescue StandardError => e
    Rails.logger.warn("Could not delete draft Stripe invoice #{invoice_id}: #{e.message}")
    nil
  end

  # Best-effort void of an unpaid invoice (never raises): a failure is only
  # logged — Stripe refuses to void a paid invoice, which is the safe outcome.
  def self.void_invoice(invoice_id)
    client.v1.invoices.void_invoice(invoice_id)
  rescue StandardError => e
    Rails.logger.warn("Could not void Stripe invoice #{invoice_id}: #{e.message}")
    nil
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
      return false
    end

    Account.find(account_id).credit!(
      credits, type: "manual_topup", description: description,
      metadata: { stripe_payment_intent_id: payment_intent_id }
    )
    true
  end

  # First member's email for the account (payment receipts) — mirrors
  # crate::email::get_account_email.
  def self.account_email(account_id)
    AccountMember.where(account_id: account_id).joins(:user).order(joined_at: :asc)
                 .pick("users.email")
  end
end
