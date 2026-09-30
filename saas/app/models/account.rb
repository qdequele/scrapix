class Account < ApplicationRecord
  TIERS = %w[free starter pro enterprise].freeze

  has_many :account_members, dependent: :delete_all
  has_many :users, through: :account_members
  has_many :api_keys, dependent: :delete_all
  has_many :account_invites, dependent: :delete_all
  has_many :crawl_configs, dependent: :delete_all
  has_many :meilisearch_engines, dependent: :delete_all
  has_many :transactions, dependent: :delete_all
  has_many :jobs

  validates :name, presence: true
  validates :tier, inclusion: { in: TIERS }

  # Atomically apply a credit movement (positive or negative) and record it.
  # Returns the ledger entry (with the new balance in #balance_after).
  def credit!(amount, type:, description: nil, metadata: nil)
    with_lock do
      update!(credits_balance: credits_balance + amount)
      transactions.create!(
        type: type, amount: amount, balance_after: credits_balance,
        description: description, metadata: metadata
      )
    end
  end

  # Debit usage reported by an engine. Idempotent on the event id (unique
  # index on transactions.metadata->>'lab_event_id'). The balance may go
  # negative (the engine pre-checks before work; charges land after it).
  # Returns [balance_before, balance_after], or nil if already debited.
  def debit_usage!(credits, lab_event_id:, operation:, description:)
    with_lock do
      return nil if transactions.where("metadata->>'lab_event_id' = ?", lab_event_id).exists?

      before = credits_balance
      update!(credits_balance: before - credits)
      transactions.create!(type: "usage_deduction", amount: -credits, balance_after: credits_balance,
                           description: "#{operation}: #{description}",
                           metadata: { lab_event_id: lab_event_id, operation: operation })
      [ before, credits_balance ]
    end
  rescue ActiveRecord::RecordNotUnique
    nil
  end

  def owner_email
    account_members.find_by(role: "owner")&.user&.email
  end

  # Mirrors scrapix_billing::check_spend_limit: this calendar month's top-ups
  # plus the requested amount must stay within monthly_spend_limit.
  def spend_limit_exceeded?(amount)
    return false unless monthly_spend_limit

    transactions.topups.this_month.sum(:amount) + amount > monthly_spend_limit
  end
end
