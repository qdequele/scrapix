class MeilisearchEngine < ApplicationRecord
  belongs_to :account

  validates :name, presence: true, uniqueness: { scope: :account_id }
  validates :url, presence: true
end
