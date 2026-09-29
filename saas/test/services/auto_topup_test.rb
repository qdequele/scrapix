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

  # --- R12: cooldown, void, retry-after-crash -------------------------------

  def usage_event
    id = SecureRandom.uuid
    LabEventReceived.create!(id: id, type: "usage.recorded", account_id: @acme.id, occurred_at: Time.current,
      payload: { "id" => id, "type" => "usage.recorded", "account_id" => @acme.id,
                 "data" => { "operation" => "scrape", "credits" => 1, "units" => {}, "description" => "x" } })
  end

  test "a failed attempt blocks the next one for an hour, then retries" do
    calls = []
    with_stub(StripeBilling, :void_invoice, ->(_id) { nil }) do
      with_stripe(Succeeded("pi_7", "requires_payment_method"), calls: calls) do
        assert_equal :failed, AutoTopup.call(@acme)
        assert_equal :skipped, AutoTopup.call(@acme), "within the hour"
        travel 61.minutes do
          assert_equal :failed, AutoTopup.call(@acme), "after the hour"
        end
      end
    end
    assert_equal 2, calls.size
  end

  test "a Stripe error starts the cooldown too" do
    calls = []
    boom = ->(**kw) { calls << kw; raise Stripe::CardError.new("Your card was declined.", "card_declined") }
    with_stub(StripeBilling, :create_and_pay_invoice, boom) do
      assert_equal :failed, AutoTopup.call(@acme)
      assert_equal :skipped, AutoTopup.call(@acme)
    end
    assert_equal 1, calls.size
  end

  test "a successful top-up doesn't block the next one" do
    calls = []
    with_stripe(Succeeded("pi_8", "succeeded"), calls: calls) do
      assert_equal :charged, AutoTopup.call(@acme)
      @acme.update!(credits_balance: 100)
      with_stripe(Succeeded("pi_9", "succeeded"), calls: calls) { assert_equal :charged, AutoTopup.call(@acme) }
    end
    assert_equal 2, calls.size
    assert_nil @acme.reload.last_auto_topup_attempt_at
  end

  test "a declined status voids the invoice" do
    voided = []
    with_stub(StripeBilling, :void_invoice, ->(id) { voided << id }) do
      with_stripe(Succeeded("pi_10", "requires_payment_method")) { assert_equal :failed, AutoTopup.call(@acme) }
    end
    assert_equal [ "in_pi_10" ], voided
  end

  test "a processing payment is neither voided nor reported as failed" do
    voided = []
    with_stub(StripeBilling, :void_invoice, ->(id) { voided << id }) do
      with_stripe(Succeeded("pi_11", "processing")) { assert_equal :failed, AutoTopup.call(@acme) }
    end
    assert_empty voided
    assert_not ScheduledEmail.exists?(email_type: "auto_topup_failed")
    assert_equal 100, @acme.reload.credits_balance
  end

  test "an error after a succeeded charge never charges again on retry" do
    calls = []
    with_stripe(Succeeded("pi_12", "succeeded"), calls: calls) do
      with_stub(StripeBilling, :add_credits_for_payment, ->(*) { raise ActiveRecord::ConnectionNotEstablished, "db down" }) do
        assert_raises(ActiveRecord::ConnectionNotEstablished) { AutoTopup.call(@acme) }
      end
      assert_equal :skipped, AutoTopup.call(@acme), "the invoice.paid webhook grants these credits"
    end
    assert_equal 1, calls.size
  end

  test "retrying an event whose top-up raised after the debit committed still tops up" do
    original = AutoTopup.method(:call)
    raised = false
    flaky = ->(account) { raised ? original.call(account) : (raised = true; raise "redis down") }
    ev = usage_event
    calls = []
    with_stripe(Succeeded("pi_13", "succeeded"), calls: calls) do
      with_stub(AutoTopup, :call, flaky) do
        LabEvents::Processor.call(ev)
        ev.reload
        assert_nil ev.processed_at
        assert_equal "redis down", ev.error
        assert_equal 99, @acme.reload.credits_balance

        LabEvents::Processor.call(ev)
      end
    end
    ev.reload
    assert ev.processed_at
    assert_nil ev.error
    assert_equal 1, calls.size
    assert_equal 5099, @acme.reload.credits_balance
    assert_equal 1, Transaction.where("metadata->>'lab_event_id' = ?", ev.id).count, "debited once"
  end

  # A card declined by /pay raises before the caller sees the invoice, so
  # create_and_pay_invoice voids it itself when asked to.
  class FakeStripe
    Obj = Struct.new(:id)
    attr_reader :voided

    def initialize(pay_error) = (@pay_error = pay_error; @voided = [])
    def v1 = self
    def invoice_items = self
    def invoices = self
    def create(*) = Obj.new("in_fake")
    def finalize_invoice(id, *) = Obj.new(id)
    def pay(*) = raise(@pay_error)
    def void_invoice(id, *) = @voided << id
    def retrieve(id, *) = Obj.new("retrieved_#{id}")
  end

  test "create_and_pay_invoice still treats 'already paid' as success and never voids it" do
    fake = FakeStripe.new(Stripe::InvalidRequestError.new("Invoice is already paid", nil))
    with_stub(StripeBilling, :client, -> { fake }) do
      invoice = StripeBilling.create_and_pay_invoice(customer_id: "cus_1", account_id: @acme.id, payment_method_id: "pm_1",
                                                     credits: 5000, amount_cents: 3500, purchase_type: "auto_topup", void_unpaid: true)
      assert_equal "retrieved_in_fake", invoice.id
    end
    assert_empty fake.voided
  end

  test "create_and_pay_invoice voids the invoice when /pay raises and void_unpaid is set" do
    fake = FakeStripe.new(Stripe::CardError.new("Your card was declined.", "card_declined"))
    with_stub(StripeBilling, :client, -> { fake }) do
      assert_raises(Stripe::CardError) do
        StripeBilling.create_and_pay_invoice(customer_id: "cus_1", account_id: @acme.id, payment_method_id: "pm_1",
                                             credits: 5000, amount_cents: 3500, purchase_type: "auto_topup", void_unpaid: true)
      end
      assert_equal [ "in_fake" ], fake.voided

      assert_raises(Stripe::CardError) do
        StripeBilling.create_and_pay_invoice(customer_id: "cus_1", account_id: @acme.id, payment_method_id: "pm_1",
                                             credits: 100, amount_cents: 100, purchase_type: "credit_purchase")
      end
      assert_equal [ "in_fake" ], fake.voided, "the purchase flow keeps its invoice"
    end
  end

  test "void_invoice is best-effort" do
    fake = Object.new
    def fake.v1 = self
    def fake.invoices = self
    def fake.void_invoice(*) = raise(Stripe::InvalidRequestError.new("already paid", nil))
    with_stub(StripeBilling, :client, -> { fake }) { assert_nil StripeBilling.void_invoice("in_x") }
  end

  test "below threshold only" do
    @acme.update!(credits_balance: 600)
    calls = []
    with_stripe(Succeeded("pi_6", "succeeded"), calls: calls) { assert_equal :skipped, AutoTopup.call(@acme) }
    assert_empty calls
  end
end
