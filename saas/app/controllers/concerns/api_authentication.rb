# Authentication mirroring the Rust engine's `auth::validate_api_key_or_session`
# middleware exactly (bins/scrapix-api/src/auth/middleware.rs): OAuth Bearer
# token, then X-API-Key, then the `scrapix_session` HS256 JWT cookie. Error
# bodies and codes match the Rust responses so the two backends are
# indistinguishable to clients.
#
# On success sets either @authenticated_account_id (API key / Bearer auth,
# account-scoped) or @authenticated_user_id (+ optional selected account from
# the X-Account-Id header, session auth).
module ApiAuthentication
  extend ActiveSupport::Concern

  class AuthenticationError < StandardError
    attr_reader :code

    def initialize(message, code)
      super(message)
      @code = code
    end
  end

  included do
    rescue_from AuthenticationError do |e|
      render json: { error: e.message, code: e.code }, status: :unauthorized
    end
  end

  private

  def authenticate_api_key_or_session!
    auth_header = request.headers["Authorization"]
    return authenticate_bearer!(auth_header.delete_prefix("Bearer ")) if auth_header&.start_with?("Bearer ")

    api_key = request.headers["X-API-Key"]
    return authenticate_api_key!(api_key) if api_key.present?

    authenticate_session!
  end

  def authenticate_bearer!(token)
    holder = OauthToken.account_for(token)
    unless holder
      raise AuthenticationError.new("Invalid or expired Bearer token", "invalid_bearer_token")
    end

    @authenticated_account_id = holder[:account_id]
    @authenticated_tier = holder[:tier]
  end

  def authenticate_api_key!(api_key)
    unless api_key.start_with?("sk_live_", "sk_test_")
      raise AuthenticationError.new("Invalid API key format", "invalid_api_key")
    end

    key_hash = Digest::SHA256.hexdigest(api_key)
    # Same Postgres function the Rust middleware calls (also bumps last_used_at).
    row = ActiveRecord::Base.connection.select_one(
      ActiveRecord::Base.sanitize_sql_array(
        [ "SELECT account_id, tier, active, api_key_id FROM validate_api_key(?)", key_hash ]
      )
    )
    raise AuthenticationError.new("Invalid or inactive API key", "invalid_api_key") if row.nil?
    raise AuthenticationError.new("Account is inactive", "account_inactive") unless row["active"]

    @authenticated_account_id = row["account_id"]
    @authenticated_tier = row["tier"]
    @authenticated_api_key_id = row["api_key_id"]
  end

  def authenticate_session!
    token = cookies["scrapix_session"]
    if token.blank?
      raise AuthenticationError.new("Missing API key or session", "not_authenticated")
    end

    begin
      claims, = JWT.decode(token, jwt_secret, true, algorithm: "HS256")
    rescue JWT::DecodeError
      raise AuthenticationError.new("Invalid or expired session", "invalid_session")
    end

    user_id = claims["sub"]
    unless user_id.to_s.match?(/\A[0-9a-f-]{36}\z/i)
      raise AuthenticationError.new("Invalid session", "invalid_session")
    end

    @authenticated_user_id = user_id
    @authenticated_email = claims["email"]
    selected = request.headers["X-Account-Id"]
    @selected_account_id = selected if selected.to_s.match?(/\A[0-9a-f-]{36}\z/i)
  end

  # Mirrors configs.rs/engines.rs `resolve_account_id`: API-key/Bearer auth is
  # account-scoped; session auth resolves via account_members (X-Account-Id
  # must be a membership; otherwise first membership wins). Both failure modes
  # map to a 404 "Account not found" like the Rust handlers.
  def resolve_account_id!
    return @authenticated_account_id if @authenticated_account_id

    if @selected_account_id
      member = AccountMember.exists?(user_id: @authenticated_user_id, account_id: @selected_account_id)
      raise_account_not_found unless member
      return @selected_account_id
    end

    membership = AccountMember.where(user_id: @authenticated_user_id).limit(1).pick(:account_id)
    raise_account_not_found unless membership
    membership
  end

  def raise_account_not_found
    raise ApiErrorRendering::ApiError.new("Account not found", "not_found")
  end

  def jwt_secret
    ENV.fetch("JWT_SECRET")
  end
end
