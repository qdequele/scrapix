class HealthController < ApplicationController
  # GET /health — liveness + database reachability.
  #
  # Served under the SaaS app's own host/prefix; the public /health stays on
  # the Rust engine until the edge routing map says otherwise.
  def show
    database = ActiveRecord::Base.connection.select_value("SELECT 1") == 1
    render json: {
      status: database ? "ok" : "degraded",
      service: "scrapix-saas",
      database: database
    }
  rescue ActiveRecord::ActiveRecordError, PG::Error
    render json: { status: "degraded", service: "scrapix-saas", database: false },
           status: :service_unavailable
  end
end
