ENV["RAILS_ENV"] ||= "test"
ENV["JWT_SECRET"] ||= "test-jwt-secret"
require_relative "../config/environment"
require "rails/test_help"

module ActiveSupport
  class TestCase
    # No parallelize: the suite runs in under a second, and the forking
    # parallelizer trips macOS Objective-C fork safety.

    # Setup all fixtures in test/fixtures/*.yml for all tests in alphabetical order.
    fixtures :all
  end
end

module SessionTestHelper
  # Session cookie identical to what login issues (HS256 JWT).
  def sign_in_as(user, account: nil)
    token = SessionToken.encode(user.id, user.email)
    cookies["scrapix_session"] = token
    @selected_account = account
  end

  def auth_headers
    @selected_account ? { "X-Account-Id" => @selected_account.id } : {}
  end
end

class ActionDispatch::IntegrationTest
  include SessionTestHelper
end
