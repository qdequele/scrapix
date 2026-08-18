require "test_helper"

class AccountInviteTest < ActiveSupport::TestCase
  test "issue! creates a pending invite" do
    invite = AccountInvite.issue!(
      account_id: accounts(:acme).id, email: "new@example.com", role: "member",
      invited_by: users(:quentin).id, token_hash: "hash-1"
    )
    assert invite.persisted?
    assert_equal "pending", invite.status
    assert_in_delta 7.days.from_now, invite.expires_at, 5.seconds
  end

  test "re-issuing updates the live invite instead of duplicating" do
    existing = account_invites(:pending_invite)
    invite = AccountInvite.issue!(
      account_id: existing.account_id, email: existing.email, role: "admin",
      invited_by: users(:quentin).id, token_hash: "rotated-hash"
    )
    assert_equal existing.id, invite.id
    assert_equal "admin", invite.reload.role
    assert_equal "rotated-hash", invite.token_hash
    assert_equal 1, AccountInvite.pending.where(email: existing.email).count
  end

  test "live scope excludes expired invites" do
    account_invites(:pending_invite).update!(expires_at: 1.hour.ago)
    assert_empty AccountInvite.live.where(id: account_invites(:pending_invite).id)
  end
end
