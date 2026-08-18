# Read-only view of Rodauth's TOTP table (the id IS the user id).
class UserOtpKey < ApplicationRecord
  self.primary_key = :id
end
