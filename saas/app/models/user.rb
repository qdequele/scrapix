class User < ApplicationRecord
  include Rodauth::Rails.model

  # Rodauth account state.
  enum :status, { unverified: 1, verified: 2, closed: 3 }

  has_many :account_members, dependent: :delete_all
  has_many :accounts, through: :account_members
  has_many :oauth_identities, dependent: :delete_all

  validates :email, presence: true, uniqueness: true

  # A user's primary account: the one they joined first (matches the Rust
  # API's `get_primary_account` ordering).
  def primary_membership
    account_members.order(joined_at: :asc).first
  end
end
