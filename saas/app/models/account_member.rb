class AccountMember < ApplicationRecord
  ROLES = %w[owner admin member viewer].freeze

  self.primary_key = [ :user_id, :account_id ]

  belongs_to :user
  belongs_to :account

  validates :role, inclusion: { in: ROLES }
end
