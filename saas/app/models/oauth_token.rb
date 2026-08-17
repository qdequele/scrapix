# OAuth access/refresh token, stored as a SHA-256 hash. Access tokens live
# 1 hour, refresh tokens 30 days and rotate on use.
class OauthToken < ApplicationRecord
  belongs_to :user

  # Resolve a raw Bearer token to the holder's account, mirroring the Rust
  # engine's validate_bearer_token: an unexpired, unrevoked access token
  # whose user has at least one account membership. Returns
  # { account_id:, tier:, user_id: } or nil.
  def self.account_for(raw_token)
    token = find_by(token_hash: Digest::SHA256.hexdigest(raw_token), token_type: "access")
    return nil if token.nil? || token.revoked? || token.expires_at.past?

    membership = AccountMember.where(user_id: token.user_id)
                              .joins(:account).limit(1)
                              .pick("accounts.id", "accounts.tier")
    return nil unless membership

    { account_id: membership[0], tier: membership[1], user_id: token.user_id }
  end
end
