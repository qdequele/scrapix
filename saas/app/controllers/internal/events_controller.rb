# POST /internal/events — product engines report usage and job lifecycle
# events here (spec 2a). Authenticated by an HMAC-SHA256 signature of the raw
# body with LAB_EVENTS_SECRET; no user auth. Stores each event once (the event
# id is the idempotency key) and acknowledges it; processing is async.
module Internal
  class EventsController < ApplicationController
    UUID = /\A\h{8}-\h{4}-\h{4}-\h{4}-\h{12}\z/

    def create
      secret = ENV["LAB_EVENTS_SECRET"]
      return head :service_unavailable if secret.blank?
      raw = request.raw_post
      expected = "sha256=#{OpenSSL::HMAC.hexdigest('SHA256', secret, raw)}"
      given = request.headers["X-Scrapix-Signature"].to_s
      return head :unauthorized unless ActiveSupport::SecurityUtils.secure_compare(expected, given)

      body = JSON.parse(raw)
      return head :bad_request unless body.is_a?(Hash)
      events = Array(body["events"])
      rows = events.filter_map { |e| row_for(e) }
      LabEventReceived.insert_all(rows, unique_by: :id) if rows.any?
      ProcessLabEventsJob.perform_later if rows.any?
      render json: { accepted: rows.map { |r| r[:id] } }
    rescue JSON::ParserError
      head :bad_request
    end

    private

    # Returns the row to insert, or nil after logging why the event was skipped.
    # A skipped event is never acknowledged, so the engine keeps retrying it;
    # the warn line is the operator's only signal that one is permanently bad.
    def row_for(e)
      return skip(nil, "event is not an object") unless e.is_a?(Hash)
      id = e["id"]
      return skip(id, "invalid id") unless id.to_s.match?(UUID)
      return skip(id, "invalid account_id") unless e["account_id"].to_s.match?(UUID)
      return skip(id, "missing type") unless e["type"].is_a?(String) && e["type"].present?
      occurred = begin
        Time.iso8601(e["occurred_at"].to_s)
      rescue ArgumentError
        nil
      end
      return skip(id, "invalid occurred_at") unless occurred
      { id: id, type: e["type"], account_id: e["account_id"], payload: e, occurred_at: occurred }
    end

    def skip(id, reason)
      Rails.logger.warn("Skipping malformed lab event#{" #{id.to_s.first(64)}" if id.present?}: #{reason}")
      nil
    end
  end
end
