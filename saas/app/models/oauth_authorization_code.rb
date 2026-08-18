# Short-lived, single-use PKCE authorization code (10-minute expiry).
class OauthAuthorizationCode < ApplicationRecord
  self.primary_key = "code"
end
