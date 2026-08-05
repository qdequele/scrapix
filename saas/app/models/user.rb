class User < ApplicationRecord
  has_many :account_members, dependent: :delete_all
  has_many :accounts, through: :account_members
  has_many :oauth_identities, dependent: :delete_all
  has_many :password_reset_tokens, dependent: :delete_all

  validates :email, presence: true, uniqueness: true

  # A user's primary account: the one they joined first (matches the Rust
  # API's `get_primary_account` ordering).
  def primary_membership
    account_members.order(joined_at: :asc).first
  end
end
