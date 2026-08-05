# Non-Stripe billing under /account/billing — port of
# bins/scrapix-api/src/auth/handlers/billing.rs (billing snapshot, tier,
# manual top-up with monthly spend-limit check, auto-topup, spend limit,
# transaction ledger). The Stripe routes (setup-intent, payment-methods,
# purchase, invoices, pricing) stay on the Rust engine until phase 7.
class BillingController < ApplicationController
  include TeamContext
  before_action :authenticate_session!

  def show
    account = Account.find_by(id: current_account_id!) ||
              api_error!("Account not found", "not_found")
    render json: {
      tier: account.tier,
      stripe_customer_id: account.stripe_customer_id,
      credits_balance: account.credits_balance,
      auto_topup_enabled: account.auto_topup_enabled,
      auto_topup_amount: account.auto_topup_amount,
      auto_topup_threshold: account.auto_topup_threshold,
      monthly_spend_limit: account.monthly_spend_limit
    }
  end

  def update
    unless Account::TIERS.include?(params[:tier].to_s)
      api_error!("Invalid tier", "validation_error")
    end
    account_id = current_account_id!
    require_role!(current_role!(account_id), %w[owner])

    Account.where(id: account_id).update_all(tier: params[:tier])
    render json: { message: "Tier updated" }
  end

  def topup
    amount = params[:amount].to_i
    api_error!("Amount must be positive", "validation_error") if amount <= 0

    account_id = current_account_id!
    check_spend_limit!(account_id, amount)

    new_balance = transaction_id = nil
    ActiveRecord::Base.transaction do
      new_balance = ActiveRecord::Base.connection.select_value(
        ActiveRecord::Base.sanitize_sql_array(
          [ "UPDATE accounts SET credits_balance = credits_balance + ? WHERE id = ? RETURNING credits_balance",
            amount, account_id ]
        )
      )
      record = Transaction.create!(
        account_id: account_id, type: "manual_topup", amount: amount,
        balance_after: new_balance, description: "Manual credit top-up"
      )
      transaction_id = record.id
    end

    render json: {
      credits_balance: new_balance,
      transaction_id: transaction_id,
      message: "Added #{amount} credits"
    }
  end

  def auto_topup
    account_id = current_account_id!
    enabled = ActiveModel::Type::Boolean.new.cast(params[:enabled])

    if enabled
      amount = params.key?(:amount) && !params[:amount].nil? ? params[:amount].to_i : 5000
      threshold = params.key?(:threshold) && !params[:threshold].nil? ? params[:threshold].to_i : 500
      if amount <= 0 || threshold.negative?
        api_error!("Amount must be positive and threshold non-negative", "validation_error")
      end
      Account.where(id: account_id).update_all(
        auto_topup_enabled: true, auto_topup_amount: amount, auto_topup_threshold: threshold
      )
      render json: { message: "Auto top-up enabled" }
    else
      Account.where(id: account_id).update_all(auto_topup_enabled: false)
      render json: { message: "Auto top-up disabled" }
    end
  end

  def spend_limit
    limit = params[:monthly_spend_limit]
    if !limit.nil? && limit.to_i <= 0
      api_error!("Spend limit must be positive", "validation_error")
    end

    account_id = current_account_id!
    Account.where(id: account_id).update_all(monthly_spend_limit: limit.nil? ? nil : limit.to_i)
    render json: {
      message: limit.nil? ? "Monthly spend limit removed" : "Monthly spend limit set to #{limit.to_i}"
    }
  end

  def transactions
    account_id = current_account_id!
    limit = [ (params[:limit].presence || 50).to_i, 200 ].min
    offset = (params[:offset].presence || 0).to_i

    scope = Transaction.where(account_id: account_id)
    rows = scope.order(created_at: :desc).limit(limit).offset(offset)
    render json: {
      transactions: rows.map { |t|
        {
          id: t.id, type: t.type, amount: t.amount, balance_after: t.balance_after,
          description: t.description, created_at: rfc3339_auto(t.created_at)
        }
      },
      total: scope.count
    }
  end

  private

  # Mirrors scrapix_billing::check_spend_limit: sum of this calendar month's
  # top-ups plus the requested amount must stay within monthly_spend_limit.
  def check_spend_limit!(account_id, amount)
    limit = Account.where(id: account_id).pick(:monthly_spend_limit)
    return unless limit

    spent = Transaction.where(account_id: account_id, type: %w[manual_topup auto_topup])
                       .where("created_at >= date_trunc('month', now())")
                       .sum(:amount)
    return unless spent + amount > limit

    api_error!("Monthly spend limit reached", "spend_limit_exceeded")
  end
end
