require "test_helper"

class BillingTest < ActionDispatch::IntegrationTest
  setup { sign_in_as users(:quentin) }

  test "billing snapshot" do
    get "/account/billing"
    assert_response :success
    assert_equal 100, response.parsed_body["credits_balance"]
    assert_equal "free", response.parsed_body["tier"]
  end

  test "manual topup credits the account and records the transaction" do
    post "/account/billing/topup", params: { amount: 250 }, as: :json
    assert_response :success
    body = response.parsed_body
    assert_equal 350, body["credits_balance"]
    assert Transaction.exists?(id: body["transaction_id"], type: "manual_topup", amount: 250)
  end

  test "topup respects the monthly spend limit" do
    sign_in_as users(:outsider)
    post "/account/billing/topup", params: { amount: 1001 }, as: :json
    assert_response :forbidden
    assert_equal "spend_limit_exceeded", response.parsed_body["code"]
  end

  test "auto topup settings round-trip" do
    patch "/account/billing/auto-topup", params: { enabled: true, amount: 9000, threshold: 900 }, as: :json
    assert_response :success
    account = accounts(:acme).reload
    assert account.auto_topup_enabled
    assert_equal 9000, account.auto_topup_amount
  end

  test "transactions are paginated newest-first" do
    accounts(:acme).credit!(10, type: "manual_topup")
    get "/account/billing/transactions", params: { limit: 1 }
    assert_response :success
    body = response.parsed_body
    assert_equal 1, body["transactions"].size
    assert_equal 2, body["total"]
    assert_equal "manual_topup", body["transactions"].first["type"]
  end
end
