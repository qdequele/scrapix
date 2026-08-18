require "test_helper"

class ConfigsTest < ActionDispatch::IntegrationTest
  setup { sign_in_as users(:quentin) }

  test "lists configs for the account" do
    get "/configs"
    assert_response :success
    assert_equal [ "daily-docs" ], response.parsed_body.map { |c| c["name"] }
  end

  test "creates a config with normalized defaults" do
    post "/configs", params: {
      name: "weekly", config: { start_urls: [ "https://example.org" ], index_uid: "weekly" }
    }, as: :json
    assert_response :created
    body = response.parsed_body
    assert_equal "weekly", body["name"]
    assert_includes body["config"].keys, "max_depth"
  end

  test "rejects duplicate names per account" do
    post "/configs", params: {
      name: "daily-docs", config: { start_urls: [ "https://example.org" ], index_uid: "x" }
    }, as: :json
    assert_response :conflict
  end

  test "cron expressions compute next_run_at" do
    patch "/configs/#{crawl_configs(:daily_docs).id}",
          params: { cron_expression: "0 3 * * *", cron_enabled: true }, as: :json
    assert_response :success
    assert crawl_configs(:daily_docs).reload.next_run_at.present?
  end

  test "deletes a config" do
    delete "/configs/#{crawl_configs(:daily_docs).id}"
    assert_response :no_content
    assert_not CrawlConfig.exists?(crawl_configs(:daily_docs).id)
  end
end
