class ApiKey < ApplicationRecord
  belongs_to :account

  validates :name, :prefix, :key_hash, presence: true

  scope :active, -> { where(active: true) }

  # API representation (contracts/src/shapes.ts API_KEY). The raw key is
  # returned once at creation, outside this shape.
  def as_json(*)
    {
      id: id,
      name: name,
      prefix: prefix,
      active: active,
      last_used_at: last_used_at&.utc&.iso8601(3),
      created_at: created_at.utc.iso8601(3)
    }
  end
end
