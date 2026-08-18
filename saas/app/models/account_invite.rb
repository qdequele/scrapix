class AccountInvite < ApplicationRecord
  ROLES = %w[admin member viewer].freeze
  STATUSES = %w[pending accepted expired revoked].freeze

  belongs_to :account
  belongs_to :inviter, class_name: "User", foreign_key: :invited_by

  validates :email, presence: true
  validates :role, inclusion: { in: ROLES }
  validates :status, inclusion: { in: STATUSES }

  scope :pending, -> { where(status: "pending") }
  scope :live, -> { pending.where(expires_at: Time.current..) }

  # API representation (contracts/src/shapes.ts INVITE).
  def as_json(*)
    {
      id: id,
      email: email,
      role: role,
      status: status,
      invited_by: invited_by,
      expires_at: expires_at.utc.iso8601(3),
      created_at: created_at.utc.iso8601(3)
    }
  end

  # Create or refresh the single live invite for (account, email). Re-inviting
  # updates the role, rotates the token, and extends the expiry — guarded by
  # the partial unique index on pending invites.
  def self.issue!(account_id:, email:, role:, invited_by:, token_hash:)
    retried = false
    begin
      invite = pending.find_or_initialize_by(account_id: account_id, email: email)
      invite.update!(
        role: role, invited_by: invited_by, token_hash: token_hash,
        expires_at: 7.days.from_now
      )
      invite
    rescue ActiveRecord::RecordNotUnique
      raise if retried

      retried = true
      retry
    end
  end
end
