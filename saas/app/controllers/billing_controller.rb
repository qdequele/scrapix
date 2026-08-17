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

    Account.find(account_id).update!(tier: params[:tier])
    render json: { message: "Tier updated" }
  end

  def topup
    amount = params[:amount].to_i
    api_error!("Amount must be positive", "validation_error") if amount <= 0

    account = Account.find(current_account_id!)
    api_error!("Monthly spend limit reached", "spend_limit_exceeded") if account.spend_limit_exceeded?(amount)

    entry = account.credit!(amount, type: "manual_topup", description: "Manual credit top-up")

    render json: {
      credits_balance: entry.balance_after,
      transaction_id: entry.id,
      message: "Added #{amount} credits"
    }
  end

  def auto_topup
    account = Account.find(current_account_id!)
    enabled = ActiveModel::Type::Boolean.new.cast(params[:enabled])

    if enabled
      amount = params.key?(:amount) && !params[:amount].nil? ? params[:amount].to_i : 5000
      threshold = params.key?(:threshold) && !params[:threshold].nil? ? params[:threshold].to_i : 500
      if amount <= 0 || threshold.negative?
        api_error!("Amount must be positive and threshold non-negative", "validation_error")
      end
      account.update!(auto_topup_enabled: true, auto_topup_amount: amount, auto_topup_threshold: threshold)
      render json: { message: "Auto top-up enabled" }
    else
      account.update!(auto_topup_enabled: false)
      render json: { message: "Auto top-up disabled" }
    end
  end

  def spend_limit
    limit = params[:monthly_spend_limit]
    if !limit.nil? && limit.to_i <= 0
      api_error!("Spend limit must be positive", "validation_error")
    end

    Account.find(current_account_id!).update!(monthly_spend_limit: limit&.to_i)
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
      transactions: rows.as_json,
      total: scope.count
    }
  end
end
