ENV["RAILS_ENV"] ||= "test"
ENV["JWT_SECRET"] ||= "test-jwt-secret"
require_relative "../config/environment"
require "rails/test_help"

module StubHelper
  # Replace `object.name` with `impl` (called with the original arguments)
  # for the duration of the block. Minitest 6 dropped minitest/mock's
  # Object#stub; this is the one piece of it the suite needs.
  def with_stub(object, name, impl)
    original = object.method(name)
    object.define_singleton_method(name) { |*args, **kw, &blk| impl.call(*args, **kw, &blk) }
    begin
      yield
    ensure
      if original.owner == object.singleton_class
        object.define_singleton_method(name, original)
      else
        object.singleton_class.send(:remove_method, name)
      end
    end
  end
end

module ActiveSupport
  class TestCase
    include StubHelper

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
