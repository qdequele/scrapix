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

      events = Array(JSON.parse(raw)["events"])
      rows = events.filter_map { |e| row_for(e) }
      LabEventReceived.insert_all(rows, unique_by: :id) if rows.any?
      ProcessLabEventsJob.perform_later if rows.any?
      render json: { accepted: rows.map { |r| r[:id] } }
    rescue JSON::ParserError
      head :bad_request
    end

    private

    def row_for(e)
      return unless e.is_a?(Hash) && e["id"].to_s.match?(UUID) && e["account_id"].to_s.match?(UUID) && e["type"].is_a?(String)
      occurred = Time.iso8601(e["occurred_at"].to_s) rescue nil
      return unless occurred
      { id: e["id"], type: e["type"], account_id: e["account_id"], payload: e, occurred_at: occurred }
    rescue StandardError => err
      Rails.logger.warn("Skipping malformed lab event: #{err.message}")
      nil
    end
  end
end
