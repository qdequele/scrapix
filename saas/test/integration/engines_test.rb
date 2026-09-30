require "test_helper"

class EnginesTest < ActionDispatch::IntegrationTest
  setup { sign_in_as users(:quentin) }

  test "engine keys are masked in responses and a masked or blank key is not written back" do
    post "/engines", params: { name: "masked", url: "http://mk:7700", api_key: "abcd1234wxyz" }, as: :json
    assert_response :created
    id = response.parsed_body["id"]
    assert_equal "••••wxyz", response.parsed_body["api_key"]
    assert response.parsed_body["has_api_key"]

    get "/engines"
    assert_equal "••••wxyz", response.parsed_body.find { |e| e["id"] == id }["api_key"]

    patch "/engines/#{id}", params: { api_key: "••••wxyz" }, as: :json
    assert_equal "abcd1234wxyz", MeilisearchEngine.find(id).api_key
    patch "/engines/#{id}", params: { name: "renamed" }, as: :json
    assert_equal "abcd1234wxyz", MeilisearchEngine.find(id).api_key
    patch "/engines/#{id}", params: { api_key: "new-key-9999" }, as: :json
    assert_equal "new-key-9999", MeilisearchEngine.find(id).api_key
  end
end
