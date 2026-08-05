class OauthIdentity < ApplicationRecord
  PROVIDERS = %w[google github].freeze

  self.table_name = "oauth_identities"

  belongs_to :user

  validates :provider, inclusion: { in: PROVIDERS }
  validates :provider_user_id, presence: true, uniqueness: { scope: :provider }
end
