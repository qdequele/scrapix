require "test_helper"

class MembersTest < ActionDispatch::IntegrationTest
  setup { sign_in_as users(:quentin) }

  test "lists members oldest-first" do
    get "/account/members"
    assert_response :success
    assert_equal %w[quentin@example.com teammate@example.com],
                 response.parsed_body.map { |m| m["email"] }
  end

  test "inviting a new email creates a live invite and queues the email" do
    assert_enqueued_emails 1 do
      post "/account/members/invite", params: { email: "someone@example.com", role: "viewer" }, as: :json
    end
    assert_response :success
    assert_equal "pending", response.parsed_body["status"]
    assert AccountInvite.live.exists?(email: "someone@example.com", role: "viewer")
  end

  test "re-inviting an email refreshes the existing invite" do
    post "/account/members/invite", params: { email: "invited@example.com", role: "admin" }, as: :json
    assert_response :success
    assert_equal account_invites(:pending_invite).id, response.parsed_body["id"]
    assert_equal "admin", account_invites(:pending_invite).reload.role
  end

  test "cannot invite an existing member" do
    post "/account/members/invite", params: { email: users(:teammate).email }, as: :json
    assert_response :conflict
    assert_equal "already_member", response.parsed_body["code"]
  end

  test "owner can change a member's role" do
    patch "/account/members/#{users(:teammate).id}", params: { role: "admin" }, as: :json
    assert_response :success
    assert_equal "admin", account_members(:teammate_acme).reload.role
  end

  test "members cannot invite" do
    sign_in_as users(:teammate)
    post "/account/members/invite", params: { email: "x@example.com" }, as: :json
    assert_response :forbidden
  end

  test "removing a member queues the notification email" do
    assert_enqueued_emails 1 do
      delete "/account/members/#{users(:teammate).id}"
    end
    assert_response :success
    assert_not AccountMember.exists?(user_id: users(:teammate).id, account_id: accounts(:acme).id)
  end

  test "the last owner cannot leave" do
    delete "/account/members/#{users(:quentin).id}"
    assert_response :bad_request
    assert_equal "last_owner", response.parsed_body["code"]
  end
end
