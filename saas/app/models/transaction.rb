class Transaction < ApplicationRecord
  TYPES = %w[
    initial_deposit manual_topup auto_topup
    usage_deduction refund adjustment
  ].freeze

  # The `type` column is the transaction kind, not Rails STI.
  self.inheritance_column = nil

  belongs_to :account

  validates :type, inclusion: { in: TYPES }

  scope :topups, -> { where(type: %w[manual_topup auto_topup]) }
  scope :this_month, -> { where(created_at: Time.current.beginning_of_month..) }

  # API representation (contracts/src/shapes.ts TRANSACTION).
  def as_json(*)
    {
      id: id,
      type: type,
      amount: amount,
      balance_after: balance_after,
      description: description,
      created_at: created_at.utc.iso8601(3)
    }
  end
end
