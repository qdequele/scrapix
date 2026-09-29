require "test_helper"

class LabEventsTest < ActiveSupport::TestCase
  include ActionMailer::TestHelper

  def receive(type: "usage.recorded", data: {}, account: accounts(:acme), id: SecureRandom.uuid)
    payload = { "id" => id, "type" => type, "occurred_at" => Time.current.utc.iso8601(3), "account_id" => account.id,
                "api_key_id" => nil, "product" => "scrapix", "data" => data }
    LabEventReceived.create!(id: id, type: type, account_id: account.id, payload: payload, occurred_at: Time.current)
  end

  def usage(credits, description: "https://e.com (#{credits} credits)", **kw)
    receive(data: { "operation" => "scrape", "credits" => credits, "units" => {}, "description" => description }, **kw)
  end

  test "usage debits the account and records the ledger entry" do
    ev = usage(3)
    LabEvents::Processor.call(ev)
    acme = accounts(:acme).reload
    assert_equal 97, acme.credits_balance
    t = acme.transactions.order(:created_at).last
    assert_equal "usage_deduction", t.type
    assert_equal(-3, t.amount)
    assert_equal "scrape: https://e.com (3 credits)", t.description
    assert_equal ev.id, t.metadata["lab_event_id"]
    assert ev.reload.processed_at
  end

  test "duplicate_event_debits_once" do
    ev = usage(3)
    LabEvents::Processor.call(ev)
    ev.update!(processed_at: nil)           # simulate a re-run
    LabEvents::Processor.call(ev)
    assert_equal 97, accounts(:acme).reload.credits_balance
    assert_equal 1, Transaction.where("metadata->>'lab_event_id' = ?", ev.id).count
  end

  test "unknown_account_is_marked_processed_with_error" do
    ghost = Account.new(id: SecureRandom.uuid)
    ev = receive(account: ghost, data: { "operation" => "scrape", "credits" => 1, "units" => {}, "description" => "x" })
    LabEvents::Processor.call(ev)
    ev.reload
    assert ev.processed_at
    assert_match(/unknown account/i, ev.error)
  end

  [ [ "a string", "3" ], [ "a float", 1.5 ], [ "negative", -1 ], [ "missing", nil ] ].each do |label, credits|
    test "invalid credits (#{label}) are marked processed with an error and never debit" do
      data = { "operation" => "scrape", "units" => {}, "description" => "x" }
      data["credits"] = credits unless credits.nil?
      ev = receive(data: data)
      assert_no_difference -> { Transaction.count } do
        LabEvents::Processor.call(ev)
      end
      ev.reload
      assert ev.processed_at, "not retried"
      assert_match(/invalid credits/i, ev.error)
      assert_equal 0, ev.attempts
      assert_equal 100, accounts(:acme).reload.credits_balance
    end
  end

  [ [ "empty", "" ], [ "not a string", 7 ], [ "missing", nil ] ].each do |label, operation|
    test "invalid operation (#{label}) is marked processed with an error and never debits" do
      data = { "credits" => 3, "units" => {}, "description" => "x" }
      data["operation"] = operation unless operation.nil?
      ev = receive(data: data)
      assert_no_difference -> { Transaction.count } do
        LabEvents::Processor.call(ev)
      end
      ev.reload
      assert ev.processed_at, "not retried"
      assert_match(/invalid operation/i, ev.error)
      assert_equal 100, accounts(:acme).reload.credits_balance
    end
  end

  test "zero-credit usage is valid and records a zero ledger entry" do
    ev = usage(0)
    LabEvents::Processor.call(ev)
    assert_nil ev.reload.error
    assert ev.processed_at
    assert_equal 100, accounts(:acme).reload.credits_balance
    assert_equal 1, Transaction.where("metadata->>'lab_event_id' = ?", ev.id).count
  end

  test "low balance email only when crossing 10" do
    accounts(:acme).update!(credits_balance: 12)
    assert_difference -> { ScheduledEmail.where(email_type: "low_balance").count }, 1 do
      LabEvents::Processor.call(usage(3))   # 12 -> 9 crosses
    end
    assert_no_difference -> { ScheduledEmail.where(email_type: "low_balance").count } do
      LabEvents::Processor.call(usage(1))   # 9 -> 8 already below
    end
    email = ScheduledEmail.find_by(email_type: "low_balance")
    assert_equal users(:quentin).email, email.recipient
    assert_equal 9, email.payload["balance"]
  end

  test "job completed queues the email when the owner opted in, once" do
    data = { "job_id" => "j1", "index_uid" => "docs", "pages_crawled" => 5, "documents_indexed" => 5, "duration_secs" => 10 }
    id = SecureRandom.uuid
    LabEvents::Processor.call(receive(type: "job.completed", data: data, id: id))
    assert_equal 1, ScheduledEmail.where(email_type: "job_completed").count
    LabEvents::Processor.call(receive(type: "job.completed", data: data)) # different event id, same job
    assert_equal 1, ScheduledEmail.where(email_type: "job_completed").count, "dedupe index per job"
  end

  test "job email respects notify_job_emails" do
    owner = accounts(:acme).account_members.find_by(role: "owner").user
    owner.update_columns(notify_job_emails: false)
    ev = receive(type: "job.failed", data: { "job_id" => "j2", "error_message" => "boom", "pages_crawled" => 0 })
    LabEvents::Processor.call(ev)
    assert_equal 0, ScheduledEmail.where(email_type: "job_failed").count
    assert ev.reload.processed_at
  end

  test "unknown event types are marked processed with an error" do
    ev = receive(type: "future.thing")
    LabEvents::Processor.call(ev)
    assert ev.reload.processed_at
    assert_match(/unknown type/i, ev.error)
  end

  test "processing errors back off and are retried" do
    ev = usage(3)
    with_stub(LabEvents::UsageDebit, :call, ->(*) { raise "db down" }) do
      LabEvents::Processor.call(ev)
    end
    ev.reload
    assert_nil ev.processed_at
    assert_equal 1, ev.attempts
    assert ev.next_attempt_at > Time.current
  end

  test "the recurring job processes pending events" do
    ev = usage(2)
    ProcessLabEventsJob.perform_now
    assert ev.reload.processed_at
    assert_equal 98, accounts(:acme).reload.credits_balance
  end
end
