class MeilisearchEngine < ApplicationRecord
  # API representation (contracts/src/shapes.ts ENGINE).
  def as_json(*)
    {
      id: id,
      account_id: account_id,
      name: name,
      url: url,
      api_key: api_key,
      is_default: is_default,
      created_at: created_at.utc.iso8601(3),
      updated_at: updated_at.utc.iso8601(3)
    }
  end

  belongs_to :account

  validates :name, presence: true, uniqueness: { scope: :account_id }
  validates :url, presence: true
end
