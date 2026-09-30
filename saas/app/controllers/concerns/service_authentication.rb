# Guards the Lab's internal service API (engine -> Lab): the caller presents
# LAB_SERVICE_TOKEN as a Bearer token. An unset token refuses everything.
module ServiceAuthentication
  extend ActiveSupport::Concern

  included { before_action :authenticate_service! }

  private

  def authenticate_service!
    expected = ENV["LAB_SERVICE_TOKEN"].to_s
    given = request.headers["Authorization"].to_s.delete_prefix("Bearer ")
    ok = expected.present? && ActiveSupport::SecurityUtils.secure_compare(expected, given)
    head :unauthorized unless ok
  end
end
