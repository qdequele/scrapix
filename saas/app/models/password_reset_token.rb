class PasswordResetToken < ApplicationRecord
  belongs_to :user

  scope :usable, -> { where(used: false).where("expires_at > now()") }
end
