# Read-only view of Rodauth's WebAuthn credentials table.
class UserWebauthnKey < ApplicationRecord
  self.primary_key = [ :account_id, :webauthn_id ]
end
