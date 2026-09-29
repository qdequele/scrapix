require "test_helper"

class ScrapixEngineTest < ActiveSupport::TestCase
  FakeHttp = Struct.new(:requests) do
    attr_writer :use_ssl, :open_timeout, :read_timeout

    def request(req)
      requests << req
      Net::HTTPOK.new("1.1", "200", "OK").tap do |res|
        res.instance_variable_set(:@read, true)
        res.instance_variable_set(:@body, '{"job_id":"j1"}')
      end
    end
  end

  def capture_request(credentials)
    http = FakeHttp.new([])
    with_stub(Net::HTTP, :new, ->(*) { http }) do
      ScrapixEngine.create_crawl({ "start_urls" => [ "https://example.com" ] }, credentials)
    end
    http.requests.first
  end

  test "service credentials send the service token and account header only" do
    ENV["LAB_SERVICE_TOKEN"] = "svc-token"
    req = capture_request(service_account_id: "acct-1", api_key: "ignored")
    assert_equal "Bearer svc-token", req["Authorization"]
    assert_equal "acct-1", req["X-Scrapix-Account-Id"]
    assert_nil req["X-API-Key"]
  ensure
    ENV.delete("LAB_SERVICE_TOKEN")
  end

  test "caller credentials are still forwarded" do
    req = capture_request(api_key: "k1", account_id: "acct-2")
    assert_equal "k1", req["X-API-Key"]
    assert_equal "acct-2", req["X-Account-Id"]
    assert_nil req["X-Scrapix-Account-Id"]
  end
end
