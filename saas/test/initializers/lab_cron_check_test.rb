require "test_helper"

class LabCronCheckTest < ActiveSupport::TestCase
  FULL = { "LAB_CRON_ENABLED" => "true", "LAB_SERVICE_TOKEN" => "t" * 32,
           "SCRAPIX_ENGINE_URL" => "http://127.0.0.1:8090" }.freeze

  test "no warning when cron is off" do
    assert_nil LabCronCheck.warning({})
    assert_nil LabCronCheck.warning({ "LAB_CRON_ENABLED" => "false" })
  end

  test "no warning when cron is on and fully configured" do
    assert_nil LabCronCheck.warning(FULL)
  end

  test "warns about each missing variable when cron is on" do
    msg = LabCronCheck.warning(FULL.except("LAB_SERVICE_TOKEN"))
    assert_includes msg, "LAB_SERVICE_TOKEN"
    assert_not_includes msg, "SCRAPIX_ENGINE_URL"

    msg = LabCronCheck.warning(FULL.merge("SCRAPIX_ENGINE_URL" => " "))
    assert_includes msg, "SCRAPIX_ENGINE_URL"
    assert_includes msg, "http://localhost:8080"

    msg = LabCronCheck.warning({ "LAB_CRON_ENABLED" => "true" })
    assert_includes msg, "LAB_SERVICE_TOKEN"
    assert_includes msg, "SCRAPIX_ENGINE_URL"
  end
end
