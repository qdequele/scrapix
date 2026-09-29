require "test_helper"

class RunDueCrawlConfigsJobTest < ActiveJob::TestCase
  setup do
    ENV["LAB_CRON_ENABLED"] = "true"
    @cfg = crawl_configs(:daily_docs)
    @cfg.update!(cron_enabled: true, cron_expression: "*/5 * * * *", next_run_at: 1.minute.ago, last_error: nil)
  end
  teardown { ENV.delete("LAB_CRON_ENABLED") }

  test "fires a due config with service credentials and advances next_run_at" do
    calls = []
    with_stub(ScrapixEngine, :create_crawl, ->(_config, creds) { calls << creds; { "job_id" => "job-9" } }) do
      RunDueCrawlConfigsJob.perform_now
    end
    assert_equal [ { service_account_id: @cfg.account_id } ], calls
    @cfg.reload
    assert_equal "job-9", @cfg.last_job_id
    assert @cfg.next_run_at > Time.current
    assert_nil @cfg.last_error
  end

  # Passes because the first run advances next_run_at, so the second finds
  # nothing due. Truly concurrent runs (several SolidQueue processes) are
  # covered by the FOR UPDATE SKIP LOCKED row lock, not by this test.
  test "fires once even if run twice back to back" do
    n = 0
    with_stub(ScrapixEngine, :create_crawl, ->(*) { n += 1; { "job_id" => "j" } }) do
      2.times { RunDueCrawlConfigsJob.perform_now }
    end
    assert_equal 1, n
  end

  test "engine errors are recorded and the schedule advances" do
    err = ScrapixEngine::EngineError.new(402, { "error" => "Insufficient credits", "code" => "insufficient_credits" })
    with_stub(ScrapixEngine, :create_crawl, ->(*) { raise err }) { RunDueCrawlConfigsJob.perform_now }
    @cfg.reload
    assert_match(/Insufficient credits/, @cfg.last_error)
    assert @cfg.next_run_at > Time.current
    assert @cfg.cron_enabled
  end

  test "an invalid config disables the schedule" do
    err = ScrapixEngine::EngineError.new(400, { "error" => "bad config", "code" => "validation_error" })
    with_stub(ScrapixEngine, :create_crawl, ->(*) { raise err }) { RunDueCrawlConfigsJob.perform_now }
    refute @cfg.reload.cron_enabled
  end

  test "does nothing unless LAB_CRON_ENABLED" do
    ENV["LAB_CRON_ENABLED"] = nil
    with_stub(ScrapixEngine, :create_crawl, ->(*) { flunk "must not fire" }) { RunDueCrawlConfigsJob.perform_now }
    assert_nil @cfg.reload.last_job_id
  end
end
