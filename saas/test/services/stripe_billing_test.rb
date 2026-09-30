require "test_helper"

class StripeBillingTest < ActiveSupport::TestCase
  # Records every Stripe call in order and models the one piece of Stripe
  # state that matters here: invoice items created without an `invoice` are
  # pending on the customer and get swept into the next invoice.
  class RecordingStripe
    Obj = Struct.new(:id)
    attr_reader :calls, :pending_items, :deleted

    def initialize(fail_on: nil)
      @fail_on = fail_on
      @calls = []
      @pending_items = []
      @deleted = []
    end

    def v1 = self
    def invoices = Invoices.new(self)
    def invoice_items = InvoiceItems.new(self)

    def record(name, *args)
      @calls << [ name, *args ]
      raise Stripe::APIConnectionError, "boom at #{name}" if @fail_on == name
    end

    class Invoices
      def initialize(stripe) = @s = stripe

      def create(params = {}, _opts = {})
        @s.record(:invoice_create, params)
        Obj.new("in_1")
      end

      def finalize_invoice(id, params = {}, _opts = {})
        @s.record(:finalize, id, params)
        Obj.new(id)
      end

      def pay(id, params = {}, _opts = {})
        @s.record(:pay, id, params)
        Obj.new(id)
      end

      def delete(id, _params = {}, _opts = {})
        @s.record(:invoice_delete, id)
        @s.deleted << id
      end

      def void_invoice(id, *) = @s.record(:void, id)
    end

    class InvoiceItems
      def initialize(stripe) = @s = stripe

      def create(params = {}, _opts = {})
        @s.record(:item_create, params)
        @s.pending_items << params if params[:invoice].nil?
        Obj.new("ii_1")
      end
    end
  end

  def pay!(fake)
    with_stub(StripeBilling, :client, -> { fake }) do
      StripeBilling.create_and_pay_invoice(customer_id: "cus_1", account_id: accounts(:acme).id, payment_method_id: "pm_1",
                                           credits: 5000, amount_cents: 3500, purchase_type: "auto_topup", void_unpaid: true)
    end
  end

  test "creates the invoice first, excluding pending items, then attaches the item to it" do
    fake = RecordingStripe.new
    assert_equal "in_1", pay!(fake).id

    assert_equal %i[invoice_create item_create finalize pay], fake.calls.map(&:first)
    invoice_params = fake.calls[0][1]
    assert_equal "exclude", invoice_params[:pending_invoice_items_behavior]
    assert_equal "cus_1", invoice_params[:customer]
    assert_equal "usd", invoice_params[:currency], "a new customer has no currency yet; never rely on the account default"
    assert_equal "pm_1", invoice_params[:default_payment_method]
    assert_equal({ scrapix_account_id: accounts(:acme).id, credits: "5000", type: "auto_topup" }, invoice_params[:metadata])
    item_params = fake.calls[1][1]
    assert_equal "in_1", item_params[:invoice]
    assert_equal "cus_1", item_params[:customer]
    assert_equal 3500, item_params[:amount]
    assert_equal "usd", item_params[:currency]
    assert_equal [ "in_1", { expand: [ "payment_intent" ] } ], fake.calls[3][1..]
    assert_empty fake.pending_items, "no item is ever left pending on the customer"
  end

  test "an invoice creation failure leaves no pending invoice item behind" do
    fake = RecordingStripe.new(fail_on: :invoice_create)
    assert_raises(Stripe::APIConnectionError) { pay!(fake) }
    assert_equal %i[invoice_create], fake.calls.map(&:first)
    assert_empty fake.pending_items
  end

  test "an invoice item failure deletes the draft invoice and re-raises" do
    fake = RecordingStripe.new(fail_on: :item_create)
    assert_raises(Stripe::APIConnectionError) { pay!(fake) }
    assert_equal %i[invoice_create item_create invoice_delete], fake.calls.map(&:first)
    assert_equal [ "in_1" ], fake.deleted
    assert_empty fake.pending_items
  end

  test "deleting the draft is best-effort and never masks the original error" do
    fake = RecordingStripe.new(fail_on: :item_create)
    def fake.invoices
      inv = super
      def inv.delete(*) = raise(Stripe::InvalidRequestError.new("nope", nil))
      inv
    end
    error = assert_raises(Stripe::APIConnectionError) { pay!(fake) }
    assert_includes error.message, "boom at item_create"
  end
end
