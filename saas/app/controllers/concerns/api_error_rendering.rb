# Error responses matching the Rust ApiError type (bins/scrapix-api/src/lib.rs):
# JSON body {error, code} with the status derived from the code.
module ApiErrorRendering
  extend ActiveSupport::Concern

  STATUS_FOR_CODE = {
    "not_found" => :not_found,
    "bad_request" => :bad_request,
    "validation_error" => :bad_request,
    "unauthorized" => :unauthorized,
    "conflict" => :conflict,
    "insufficient_credits" => :payment_required,
    "spend_limit_exceeded" => :forbidden,
    "service_unavailable" => :service_unavailable
  }.freeze

  class ApiError < StandardError
    attr_reader :code

    def initialize(message, code)
      super(message)
      @code = code
    end
  end

  included do
    rescue_from ApiError do |e|
      render json: { error: e.message, code: e.code },
             status: STATUS_FOR_CODE.fetch(e.code, :internal_server_error)
    end
  end

  private

  def api_error!(message, code)
    raise ApiError.new(message, code)
  end
end
