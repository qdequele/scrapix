# Credential checks shared by the public API (ApiAuthentication) and the
# engine's introspection endpoint (Internal::IntrospectionsController), so
# the two can never disagree. Every method returns nil for an invalid
# credential and never raises for bad input.
module CredentialResolver
  UUID = /\A\h{8}-\h{4}-\h{4}-\h{4}-\h{12}\z/

  module_function

  # validate_api_key() also bumps api_keys.last_used_at.
  def api_key(raw)
    return nil unless raw.to_s.start_with?("sk_live_", "sk_test_")
    row = ActiveRecord::Base.connection.select_one(
      ActiveRecord::Base.sanitize_sql_array(
        [ "SELECT account_id, tier, active, api_key_id FROM validate_api_key(?)", Digest::SHA256.hexdigest(raw) ]
      )
    )
    return nil if row.nil?
    { account_id: row["account_id"], tier: row["tier"], api_key_id: row["api_key_id"], active: row["active"] }
  end

  def oauth(raw)
    OauthToken.account_for(raw.to_s)
  end

  def session_user(raw)
    claims, = JWT.decode(raw.to_s, ENV.fetch("JWT_SECRET"), true, algorithm: "HS256")
    return nil unless claims["sub"].to_s.match?(UUID)
    { user_id: claims["sub"], email: claims["email"] }
  rescue JWT::DecodeError
    nil
  end

  # The selected membership, or the first one when account_id is nil.
  def membership(user_id, account_id)
    scope = AccountMember.joins(:account).where(user_id: user_id)
    if account_id
      return nil unless account_id.to_s.match?(UUID)
      scope = scope.where(account_id: account_id)
    end
    row = scope.limit(1).pick("accounts.id", "accounts.tier", "account_members.role", "accounts.active")
    row && { account_id: row[0], tier: row[1], role: row[2], active: row[3] }
  end
end
