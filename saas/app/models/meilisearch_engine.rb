class MeilisearchEngine < ApplicationRecord
  MASK = "••••".freeze

  encrypts :api_key

  # API representation (contracts/src/shapes.ts ENGINE). The key itself is
  # never returned: only a masked hint (last 4 characters).
  def as_json(*)
    {
      id: id,
      account_id: account_id,
      name: name,
      url: url,
      api_key: masked_api_key,
      has_api_key: api_key.present?,
      is_default: is_default,
      created_at: created_at.utc.iso8601(3),
      updated_at: updated_at.utc.iso8601(3)
    }
  end

  def masked_api_key
    api_key.present? ? "#{MASK}#{api_key.to_s.last(4)}" : ""
  end

  # A submitted value that should leave the stored key alone: absent, blank,
  # or the masked hint echoed back by a form.
  def self.keep_key?(value)
    value.nil? || value.to_s.empty? || value.to_s.start_with?(MASK)
  end

  belongs_to :account

  validates :name, presence: true, uniqueness: { scope: :account_id }
  validates :url, presence: true
end
