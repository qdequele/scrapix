require "test_helper"

class StripeWebhooksTest < ActionDispatch::IntegrationTest
  SECRET = "whsec_test".freeze

  # Stands in for StripeBilling.client: `invoices.retrieve` returns the
  # invoice as the pinned API version renders it (with its payment intent).
  class FakeStripe
    Obj = Struct.new(:id, :payment_intent)
    attr_reader :retrieved

    def initialize(payment_intent) = (@payment_intent = payment_intent; @retrieved = [])
    def v1 = self
    def invoices = self

    def retrieve(id, params = {}, _opts = {})
      @retrieved << [ id, params ]
      Obj.new(id, @payment_intent)
    end
  end

  setup do
    @acme = accounts(:acme)
    ENV["STRIPE_SECRET_KEY"] = "sk_test_x"
    ENV["STRIPE_WEBHOOK_SECRET"] = SECRET
  end

  teardown do
    ENV.delete("STRIPE_SECRET_KEY")
    ENV.delete("STRIPE_WEBHOOK_SECRET")
  end

  # An invoice.paid event as a post-2025-03-31 endpoint sends it: no
  # `payment_intent` field on the invoice.
  def invoice_paid(credits: 5000)
    {
      id: "evt_1", object: "event", type: "invoice.paid",
      data: { object: { id: "in_1", object: "invoice", amount_paid: 3500,
                        metadata: { scrapix_account_id: @acme.id, credits: credits.to_s, type: "auto_topup" } } }
    }.to_json
  end

  def deliver(payload, fake)
    now = Time.current
    header = Stripe::Webhook::Signature.generate_header(now, Stripe::Webhook::Signature.compute_signature(now, payload, SECRET))
    with_stub(StripeBilling, :client, -> { fake }) do
      post "/webhooks/stripe", params: payload, headers: { "Stripe-Signature" => header, "Content-Type" => "application/json" }
    end
  end

  def pi_credits(pi_id)
    Transaction.where("metadata->>'stripe_payment_intent_id' = ?", pi_id)
  end

  test "invoice.paid without payment_intent retrieves the invoice and credits under the payment intent" do
    fake = FakeStripe.new(FakeStripe::Obj.new("pi_1"))
    deliver(invoice_paid, fake)
    assert_response :ok
    assert_equal [ [ "in_1", { expand: [ "payment_intent" ] } ] ], fake.retrieved
    assert_equal 5100, @acme.reload.credits_balance
    assert_equal 1, pi_credits("pi_1").count
    assert_equal 0, pi_credits("in_1").count, "never keyed on the invoice id"
  end

  test "invoice.paid after auto top-up already credited the payment intent credits nothing more" do
    StripeBilling.add_credits_for_payment(@acme.id, 5000, "pi_1", "Auto top-up (Stripe)")
    assert_equal 5100, @acme.reload.credits_balance

    deliver(invoice_paid, FakeStripe.new("pi_1")) # an unexpanded id works too
    assert_response :ok
    assert_equal 5100, @acme.reload.credits_balance
    assert_equal 1, pi_credits("pi_1").count
  end

  test "invoice.paid whose invoice has no payment intent credits nothing" do
    deliver(invoice_paid, FakeStripe.new(nil))
    assert_response :ok
    assert_equal 100, @acme.reload.credits_balance
    assert_equal 0, pi_credits("in_1").count
  end

  test "payment_intent.succeeded and invoice.paid for one payment credit once, under the payment intent" do
    pi_event = {
      id: "evt_2", object: "event", type: "payment_intent.succeeded",
      data: { object: { id: "pi_1", object: "payment_intent", amount: 3500,
                        metadata: { scrapix_account_id: @acme.id, credits: "5000" } } }
    }.to_json
    fake = FakeStripe.new(FakeStripe::Obj.new("pi_1"))
    deliver(pi_event, fake)
    assert_response :ok
    deliver(invoice_paid, fake)
    assert_response :ok
    assert_equal 5100, @acme.reload.credits_balance
    assert_equal 1, pi_credits("pi_1").count
  end

  test "a Stripe error while retrieving the invoice is a 500 so Stripe retries" do
    fake = FakeStripe.new(nil)
    def fake.retrieve(*) = raise(Stripe::APIConnectionError, "down")
    deliver(invoice_paid, fake)
    assert_response :internal_server_error
    assert_equal 100, @acme.reload.credits_balance
  end
end
