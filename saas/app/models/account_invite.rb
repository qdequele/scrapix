class AccountInvite < ApplicationRecord
  ROLES = %w[admin member viewer].freeze
  STATUSES = %w[pending accepted expired revoked].freeze

  belongs_to :account
  belongs_to :inviter, class_name: "User", foreign_key: :invited_by

  validates :email, presence: true
  validates :role, inclusion: { in: ROLES }
  validates :status, inclusion: { in: STATUSES }

  scope :pending, -> { where(status: "pending") }
end
