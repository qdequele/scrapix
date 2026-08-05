# Stripe billing routes under /account/billing — port of
# bins/scrapix-api/src/stripe.rs session routes. Like the Rust API (which only
# mounts these when STRIPE_SECRET_KEY is set), every route 404s when Stripe is
# not configured.
class StripeBillingController < ApplicationController
  include TeamContext
  before_action :require_stripe
  before_action :authenticate_session!

  rescue_from Stripe::StripeError do |e|
    Rails.logger.error("Stripe error: #{e.message}")
    render json: { error: stripe_error_message(e), code: "stripe_error" },
           status: :internal_server_error
  end

  def setup_intent
    account_id = current_account_id!
    customer_id = StripeBilling.get_or_create_customer(account_id)

    intent = StripeBilling.client.v1.setup_intents.create(
      customer: customer_id,
      payment_method_types: [ "card" ],
      metadata: { scrapix_account_id: account_id }
    )
    api_error!("Missing client_secret", "stripe_error") unless intent.client_secret

    render json: { client_secret: intent.client_secret }
  end

  def payment_methods
    account_id = current_account_id!
    account = Account.find_by(id: account_id)
    return render json: [] if account&.stripe_customer_id.blank?

    default_pm = account.stripe_default_payment_method_id
    methods = StripeBilling.client.v1.payment_methods.list(
      customer: account.stripe_customer_id, type: "card"
    )
    render json: methods.data.map { |pm|
      card = pm.card
      {
        id: pm.id,
        brand: card&.brand&.downcase,
        last4: card&.last4,
        exp_month: card&.exp_month,
        exp_year: card&.exp_year,
        is_default: default_pm == pm.id
      }
    }
  end

  def delete_payment_method
    account_id = current_account_id!
    account = Account.find_by(id: account_id)
    api_error!("No Stripe customer", "no_customer") if account&.stripe_customer_id.blank?

    pm =
      begin
        StripeBilling.client.v1.payment_methods.retrieve(params[:id])
      rescue Stripe::StripeError
        api_error!("Payment method not found", "not_found")
      end

    pm_customer = pm.customer.is_a?(String) ? pm.customer : pm.customer&.id
    if pm_customer != account.stripe_customer_id
      api_error!("Payment method does not belong to this account", "forbidden")
    end

    StripeBilling.client.v1.payment_methods.detach(pm.id)
    if account.stripe_default_payment_method_id == pm.id
      Account.where(id: account_id).update_all(stripe_default_payment_method_id: nil)
    end

    render json: { message: "Payment method removed" }
  end

  def set_default_payment_method
    account_id = current_account_id!
    Account.where(id: account_id)
           .update_all(stripe_default_payment_method_id: params[:payment_method_id].to_s)
    render json: { message: "Default payment method updated" }
  end

  def purchase
    credits = params[:credits].to_i
    api_error!("Minimum purchase is 100 credits", "validation_error") if credits < 100

    amount_cents = StripeBilling.calculate_price_cents(credits)
    account_id = current_account_id!
    customer_id = StripeBilling.get_or_create_customer(account_id)

    pm_id = params[:payment_method_id].presence ||
            Account.where(id: account_id).pick(:stripe_default_payment_method_id) ||
            api_error!("No payment method on file. Please add a card first.", "no_payment_method")

    invoice =
      begin
        StripeBilling.create_and_pay_invoice(
          customer_id: customer_id, account_id: account_id,
          payment_method_id: pm_id, credits: credits,
          amount_cents: amount_cents, purchase_type: "credit_purchase"
        )
      rescue Stripe::StripeError => e
        Rails.logger.error("Stripe invoice flow failed: #{e.message}")
        api_error!("Payment failed. Please try again or use a different card.", "stripe_error")
      end

    pi = invoice.payment_intent
    case pi&.status
    when "succeeded"
      StripeBilling.add_credits_for_payment(account_id, credits, pi.id, "Credit purchase")
      render json: {
        status: "succeeded", client_secret: nil, credits: credits,
        amount_cents: amount_cents, message: "#{credits} credits added to your account"
      }
    when "requires_action"
      render json: {
        status: "requires_action", client_secret: pi.client_secret, credits: credits,
        amount_cents: amount_cents, message: "Additional authentication required"
      }
    else
      Rails.logger.warn("Unexpected payment status #{pi&.status} on invoice #{invoice.id}")
      api_error!("Payment could not be processed", "payment_failed")
    end
  end

  def invoices
    account_id = current_account_id!
    customer_id = Account.where(id: account_id).pick(:stripe_customer_id)
    return render json: [] if customer_id.blank?

    result = StripeBilling.client.v1.invoices.list(
      customer: customer_id, status: "paid", limit: 50
    )
    render json: result.data.map { |inv|
      {
        id: inv.id,
        number: inv.number,
        amount_cents: inv.amount_paid || 0,
        credits: inv.metadata&.[]("credits")&.then { |c| Integer(c, exception: false) },
        status: inv.status || "unknown",
        description: inv.description,
        created_at: inv.created ? Time.at(inv.created).utc.strftime("%Y-%m-%dT%H:%M:%S+00:00") : "",
        invoice_pdf: inv.invoice_pdf,
        hosted_invoice_url: inv.hosted_invoice_url
      }
    }
  end

  def pricing
    render json: StripeBilling::PRICING_TIERS
  end

  private

  def require_stripe
    head :not_found unless StripeBilling.configured?
  end

  def stripe_error_message(error)
    case action_name
    when "setup_intent" then "Failed to create setup intent"
    when "payment_methods" then "Failed to list payment methods"
    when "invoices" then "Failed to list invoices"
    else "Stripe request failed"
    end
  end
end
