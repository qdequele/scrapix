require "test_helper"

class AccountTest < ActiveSupport::TestCase
  test "credit! updates the balance and writes a ledger entry" do
    account = accounts(:acme)
    entry = account.credit!(500, type: "manual_topup", description: "test topup")

    assert_equal 600, entry.balance_after
    assert_equal 600, account.reload.credits_balance
    assert_equal "manual_topup", entry.type
    assert_equal 500, entry.amount
  end

  test "credit! accepts negative movements" do
    entry = accounts(:acme).credit!(-40, type: "usage_deduction")
    assert_equal 60, entry.balance_after
  end

  test "spend_limit_exceeded? is false without a limit" do
    assert_not accounts(:acme).spend_limit_exceeded?(1_000_000)
  end

  test "spend_limit_exceeded? counts this month's topups against the limit" do
    account = accounts(:globex)
    account.credit!(800, type: "manual_topup")

    assert_not account.spend_limit_exceeded?(200)
    assert account.spend_limit_exceeded?(201)
  end

  test "rejects unknown tiers" do
    assert_not Account.new(name: "x", tier: "platinum").valid?
  end
end
