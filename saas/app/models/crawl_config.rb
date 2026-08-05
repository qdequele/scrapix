class CrawlConfig < ApplicationRecord
  belongs_to :account

  validates :name, presence: true, uniqueness: { scope: :account_id }
  validates :config, presence: true
end
