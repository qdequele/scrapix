class Transaction < ApplicationRecord
  TYPES = %w[
    initial_deposit manual_topup auto_topup
    usage_deduction refund adjustment
  ].freeze

  # The `type` column is the transaction kind, not Rails STI.
  self.inheritance_column = nil

  belongs_to :account

  validates :type, inclusion: { in: TYPES }
end
