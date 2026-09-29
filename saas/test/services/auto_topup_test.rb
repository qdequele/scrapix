require "test_helper"

class AutoTopupTest < ActiveSupport::TestCase
  # StripeBilling.create_and_pay_invoice returns a Stripe invoice whose
  # expanded `payment_intent` has `id` and `status` (see StripeBillingController#purchase).
  PaymentIntent = Struct.new(:id, :status)
  Invoice = Struct.new(:id, :payment_intent)
  def Succeeded(pi_id, status) = Invoice.new("in_#{pi_id}", PaymentIntent.new(pi_id, status))

  setup do
    @acme = accounts(:acme)
    @acme.update!(auto_topup_enabled: true, auto_topup_amount: 5000, auto_topup_threshold: 500, credits_balance: 100,
                  stripe_customer_id: "cus_1", stripe_default_payment_method_id: "pm_1")
    ENV["STRIPE_SECRET_KEY"] = "sk_test_x"
  end
  teardown { ENV.delete("STRIPE_SECRET_KEY") }

  def with_stripe(result, calls: [])
    with_stub(StripeBilling, :create_and_pay_invoice, ->(**kw) { calls << kw; result }) { yield }
  end

  test "charges and grants credits only after a succeeded payment" do
    calls = []
    with_stripe(Succeeded("pi_1", "succeeded"), calls: calls) { assert_equal :charged, AutoTopup.call(@acme) }
    assert_equal 1, calls.size
    assert_equal 5100, @acme.reload.credits_balance
    receipt = ScheduledEmail.find_by(email_type: "auto_topup_receipt")
    assert receipt
    assert_equal 5100, receipt.payload["new_balance"]
    assert_equal "pi_1", @acme.transactions.order(:created_at).last.metadata["stripe_payment_intent_id"]
  end

  test "failed payment grants nothing and emails the failure" do
    with_stripe(Succeeded("pi_2", "requires_payment_method")) { assert_equal :failed, AutoTopup.call(@acme) }
    assert_equal 100, @acme.reload.credits_balance
    assert ScheduledEmail.exists?(email_type: "auto_topup_failed")
  end

  test "stripe errors grant nothing and email the failure" do
    boom = ->(**) { raise Stripe::CardError.new("Your card was declined.", "card_declined") }
    with_stub(StripeBilling, :create_and_pay_invoice, boom) { assert_equal :failed, AutoTopup.call(@acme) }
    assert_equal 100, @acme.reload.credits_balance
    assert_equal "Your card was declined.", ScheduledEmail.find_by(email_type: "auto_topup_failed").payload["reason"]
  end

  test "no card on file: no charge, failure email at most once a day" do
    @acme.update!(stripe_default_payment_method_id: nil)
    calls = []
    with_stripe(Succeeded("pi_3", "succeeded"), calls: calls) { 2.times { AutoTopup.call(@acme) } }
    assert_empty calls
    assert_equal 1, ScheduledEmail.where(email_type: "auto_topup_failed").count
  end

  test "respects the monthly spend limit" do
    @acme.update!(monthly_spend_limit: 1000)
    calls = []
    with_stripe(Succeeded("pi_4", "succeeded"), calls: calls) { assert_equal :failed, AutoTopup.call(@acme) }
    assert_empty calls
  end

  test "back_to_back_debits_charge_once" do
    calls = []
    with_stripe(Succeeded("pi_5", "succeeded"), calls: calls) do
      2.times do
        LabEvents::Processor.call(LabEventReceived.create!(id: (id = SecureRandom.uuid), type: "usage.recorded", account_id: @acme.id,
          occurred_at: Time.current, payload: { "id" => id, "type" => "usage.recorded", "account_id" => @acme.id,
          "data" => { "operation" => "scrape", "credits" => 1, "units" => {}, "description" => "x" } }))
      end
    end
    assert_equal 1, calls.size, "second debit sees the topped-up balance"
  end

  test "below threshold only" do
    @acme.update!(credits_balance: 600)
    calls = []
    with_stripe(Succeeded("pi_6", "succeeded"), calls: calls) { assert_equal :skipped, AutoTopup.call(@acme) }
    assert_empty calls
  end
end
