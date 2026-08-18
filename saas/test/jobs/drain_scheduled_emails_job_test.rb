require "test_helper"

class DrainScheduledEmailsJobTest < ActiveJob::TestCase
  include ActionMailer::TestHelper

  test "delivers due rows and marks them sent" do
    row = ScheduledEmail.create!(
      email_type: "job_completed", recipient: "u@example.com",
      payload: { "job_id" => "j1", "index_uid" => "docs", "pages_crawled" => 5,
                 "documents_indexed" => 5, "duration_secs" => 10 },
      send_at: Time.current
    )

    assert_emails 1 do
      DrainScheduledEmailsJob.perform_now
    end
    assert row.reload.sent
    assert_equal 1, row.attempts
  end

  test "leaves future rows alone" do
    ScheduledEmail.create!(
      email_type: "welcome", recipient: "u@example.com",
      payload: { "name" => "Q" }, send_at: 1.hour.from_now
    )
    assert_no_emails { DrainScheduledEmailsJob.perform_now }
  end

  test "unknown types record the error and back off" do
    row = ScheduledEmail.create!(
      email_type: "mystery", recipient: "u@example.com", payload: {}, send_at: Time.current
    )
    DrainScheduledEmailsJob.perform_now

    row.reload
    assert_not row.sent
    assert_match(/Unknown scheduled email type/, row.last_error)
    assert row.next_attempt_at.present?
  end

  test "rows past max attempts are skipped" do
    row = ScheduledEmail.create!(
      email_type: "mystery", recipient: "u@example.com", payload: {},
      send_at: Time.current, attempts: ScheduledEmail::MAX_ATTEMPTS
    )
    assert_no_emails { DrainScheduledEmailsJob.perform_now }
    assert_equal ScheduledEmail::MAX_ATTEMPTS, row.reload.attempts
  end
end
