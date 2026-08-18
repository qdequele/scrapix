class ApplicationController < ActionController::API
  include ActionController::Cookies
  include ApiErrorRendering
  include ApiAuthentication
end
