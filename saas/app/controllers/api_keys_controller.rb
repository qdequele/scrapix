# API key management under /account/api-keys — port of
# bins/scrapix-api/src/auth/handlers/api_keys.rs. The full key is returned
# exactly once at creation; only the SHA-256 hash is stored, which the Rust
# engine keeps validating via the shared validate_api_key() function.
class ApiKeysController < ApplicationController
  include TeamContext
  before_action :authenticate_session!

  def index
    account_id = current_account_id!
    render json: ApiKey.where(account_id: account_id).order(created_at: :desc)
  end

  def create
    name = params[:name].to_s
    api_error!("Name is required", "validation_error") if name.strip.empty?

    account_id = current_account_id!
    require_role!(current_role!(account_id), %w[owner admin])

    api_key = "sk_live_#{SecureRandom.alphanumeric(32)}"
    prefix = "#{api_key[0, 12]}..."

    record = ApiKey.create!(
      account_id: account_id,
      name: name.strip,
      prefix: prefix,
      key_hash: Digest::SHA256.hexdigest(api_key)
    )

    render json: { id: record.id, name: name, prefix: prefix, key: api_key }
  end

  def revoke
    account_id = current_account_id!
    require_role!(current_role!(account_id), %w[owner admin])
    api_error!("Invalid key ID", "validation_error") unless uuid?(params[:id])

    key = ApiKey.find_by(id: params[:id], account_id: account_id)
    api_error!("Key not found", "not_found") unless key
    key.update!(active: false)

    render json: { message: "Key revoked" }
  end
end
