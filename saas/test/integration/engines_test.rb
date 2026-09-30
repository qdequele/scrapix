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

  test "changing the URL requires re-entering the API key" do
    post "/engines", params: { name: "moving", url: "http://mv:7700", api_key: "stored-key-1234" }, as: :json
    id = response.parsed_body["id"]

    [ { url: "http://attacker:7700" }, { url: "http://attacker:7700", api_key: "••••1234" } ].each do |body|
      patch "/engines/#{id}", params: body, as: :json
      assert_response :bad_request
      assert_equal "validation_error", response.parsed_body["code"]
      assert_equal "Changing the URL requires re-entering the API key", response.parsed_body["error"]
      engine = MeilisearchEngine.find(id)
      assert_equal [ "http://mv:7700", "stored-key-1234" ], [ engine.url, engine.api_key ]
    end

    patch "/engines/#{id}", params: { url: "http://mv2:7700", api_key: "new-key-5678" }, as: :json
    assert_response :success
    assert_equal [ "http://mv2:7700", "new-key-5678" ], MeilisearchEngine.find(id).then { [ _1.url, _1.api_key ] }

    patch "/engines/#{id}", params: { url: "http://mv3:7700", api_key: "" }, as: :json
    assert_response :success
    assert_equal [ "http://mv3:7700", "" ], MeilisearchEngine.find(id).then { [ _1.url, _1.api_key ] }

    # No stored key: nothing to leak, the URL can change on its own.
    patch "/engines/#{id}", params: { url: "http://mv4:7700" }, as: :json
    assert_response :success
    assert_equal "http://mv4:7700", MeilisearchEngine.find(id).url
  end
end
