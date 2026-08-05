class ApplicationController < ActionController::API
  include ActionController::Cookies
  include ApiErrorRendering
  include ApiAuthentication

  private

  # chrono's `to_rfc3339()` (SecondsFormat::AutoSi, no Z): fractional seconds
  # rendered with 0, 3, 6, or 9 digits depending on precision, offset +00:00.
  def rfc3339_auto(time)
    time = time.utc
    nanos = time.nsec
    fraction =
      if nanos.zero? then ""
      elsif (nanos % 1_000_000).zero? then format(".%03d", nanos / 1_000_000)
      elsif (nanos % 1_000).zero? then format(".%06d", nanos / 1_000)
      else format(".%09d", nanos)
      end
    "#{time.strftime('%Y-%m-%dT%H:%M:%S')}#{fraction}+00:00"
  end
end
