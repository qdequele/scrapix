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
end
