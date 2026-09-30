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

  test "keeps index_uid, source and document/OCR features on create" do
    post "/configs", params: {
      name: "named-index",
      config: {
        start_urls: [ "https://example.org" ],
        index_uid: "my-custom-index",
        source: "docs",
        features: { documents: { enabled: true }, ocr: { mode: "auto" } }
      }
    }, as: :json
    assert_response :created
    config = response.parsed_body["config"]
    assert_equal "my-custom-index", config["index_uid"]
    assert_equal "docs", config["source"]
    assert_equal({ "enabled" => true }, config["features"]["documents"])
    assert_equal({ "mode" => "auto" }, config["features"]["ocr"])
  end

  test "updates index_uid" do
    record = crawl_configs(:daily_docs)
    patch "/configs/#{record.id}", params: {
      config: { start_urls: [ "https://example.org" ], index_uid: "renamed-index" }
    }, as: :json
    assert_response :success
    assert_equal "renamed-index", record.reload.config["index_uid"]
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
