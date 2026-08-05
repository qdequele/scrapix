class ApiKey < ApplicationRecord
  belongs_to :account

  validates :name, :prefix, :key_hash, presence: true

  scope :active, -> { where(active: true) }
end
