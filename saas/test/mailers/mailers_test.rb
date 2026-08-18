require "test_helper"

class MailersTest < ActionMailer::TestCase
  test "verification email carries the token link" do
    mail = AuthMailer.with(to: "u@example.com", name: "Q", token: "tok123").verification
    assert_equal "Verify your email address — Scrapix", mail.subject
    assert_equal [ "u@example.com" ], mail.to
    assert_includes mail.body.encoded, "verify-email?token=3D\"tok123\"".sub("=3D\"", "=") # quoted-printable tolerant
    assert_includes mail.body.encoded, "Verify Email Address"
  end

  test "team invite renders account, role, and invite link" do
    mail = TeamMailer.with(to: "u@example.com", account_name: "Acme <3", inviter_name: "Q",
                           role: "member", token: "tok").invite
    assert_equal "You're invited to join Acme <3 on Scrapix", mail.subject
    assert_includes mail.body.encoded, "Acme &lt;3"
    assert_includes mail.body.encoded, "Accept Invite"
  end

  test "payment receipt formats dollars" do
    mail = BillingMailer.with(to: "u@example.com", credits: 5000, amount_cents: 3500).payment_receipt
    assert_equal "Payment receipt — 5000 credits", mail.subject
    assert_includes mail.body.encoded, "$35.00"
  end

  test "job completed formats the duration" do
    mail = JobsMailer.with(to: "u@example.com", job_id: "job-1", index_uid: "docs",
                           pages_crawled: 10, documents_indexed: 9, duration_secs: 95).completed
    assert_includes mail.body.encoded, "1m 35s"
    assert_includes mail.body.encoded, "job-1"
  end

  test "job failed escapes the error message" do
    mail = JobsMailer.with(to: "u@example.com", job_id: "job-1",
                           error_message: "<script>alert(1)</script>", pages_crawled: 2).failed
    assert_not_includes mail.body.encoded, "<script>"
  end
end
