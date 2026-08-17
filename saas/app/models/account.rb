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

  # Mirrors scrapix_billing::check_spend_limit: this calendar month's top-ups
  # plus the requested amount must stay within monthly_spend_limit.
  def spend_limit_exceeded?(amount)
    return false unless monthly_spend_limit

    transactions.topups.this_month.sum(:amount) + amount > monthly_spend_limit
  end
end
