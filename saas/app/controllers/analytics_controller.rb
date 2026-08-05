# Tinybird-style analytics pipes over ClickHouse.
#
# Byte-compatible port of the Rust implementation (bins/scrapix-api/src/
# analytics.rs + crates/scrapix-storage/src/clickhouse.rs). The SQL, meta
# arrays, row math, and timestamp formats are pinned by contracts/ and must
# not drift — see contracts/tests/analytics.contract.test.ts and the parity
# fixtures in SCR-85.
#
# Notable quirks preserved on purpose:
# - job_stats/job_timeline/job_event_summary timestamps use Rust's
#   `time::OffsetDateTime` Display format ("2026-08-04 5:07:09.0 +00:00:00",
#   hour not zero-padded); hourly_stats uses RFC3339; daily uses "YYYY-MM-DD".
# - job_timeline's meta declares only 3 columns while rows carry 12 fields.
# - domain_stats and account_usage return a single all-zeros row when there
#   is no data; job_stats returns zero rows.
# - kpis aggregates the top_domains query (limit 10000) in application code.
class AnalyticsController < ApplicationController
  before_action :require_clickhouse

  rescue_from ClickhouseClient::QueryError do |e|
    Rails.logger.error("analytics query failed: #{e.message}")
    render json: { error: e.message, code: "QUERY_ERROR" }, status: :internal_server_error
  end

  DOMAIN_META = [
    { name: "domain", type: "String" },
    { name: "total_requests", type: "UInt64" },
    { name: "successful_requests", type: "UInt64" },
    { name: "failed_requests", type: "UInt64" },
    { name: "success_rate", type: "Float64" },
    { name: "avg_duration_ms", type: "Float64" },
    { name: "total_bytes", type: "UInt64" }
  ].freeze

  USAGE_META_TAIL = [
    { name: "total_bytes", type: "UInt64" },
    { name: "avg_duration_ms", type: "Float64" },
    { name: "unique_domains", type: "UInt64" },
    { name: "js_renders", type: "UInt64" },
    { name: "ai_prompt_tokens", type: "UInt64" },
    { name: "ai_completion_tokens", type: "UInt64" }
  ].freeze

  # Shared SELECT list for request_events success/failure aggregation.
  REQUEST_AGG = <<~SQL.freeze
    sum(pages_fetched) as total_requests,
    sumIf(pages_fetched, status_code >= 200 AND status_code < 400) as successful_requests,
    sumIf(pages_fetched, status_code >= 400 OR status_code = 0 OR error != '') as failed_requests
  SQL

  def pipes
    render json: PIPES
  end

  def top_domains
    started = monotonic_now
    data = top_domains_rows(hours_param, limit_param(20))
    render_envelope(DOMAIN_META, data, started)
  end

  def domain_stats
    started = monotonic_now
    domain = params.require(:domain)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { domain: domain, hours: hours_param })
      SELECT
          domain,
          #{REQUEST_AGG},
          avg(duration_ms) as avg_duration_ms,
          sum(content_length) as total_bytes
      FROM request_events
      WHERE domain = {domain:String} AND timestamp >= now() - INTERVAL {hours:UInt32} HOUR
      GROUP BY domain
    SQL
    row = rows.first || {
      "domain" => domain, "total_requests" => 0, "successful_requests" => 0,
      "failed_requests" => 0, "avg_duration_ms" => 0.0, "total_bytes" => 0
    }
    render_envelope(DOMAIN_META, [ domain_row(row) ], started)
  end

  def hourly_stats
    started = monotonic_now
    rows = ClickhouseClient.instance.query(<<~SQL, params: { hours: hours_param })
      SELECT
          toStartOfHour(timestamp) as hour,
          sum(pages_fetched) as requests,
          sumIf(pages_fetched, status_code >= 200 AND status_code < 400) as successes,
          sumIf(pages_fetched, status_code >= 400 OR status_code = 0 OR error != '') as failures,
          avg(duration_ms) as avg_duration_ms,
          sum(content_length) as total_bytes
      FROM request_events
      WHERE timestamp >= now() - INTERVAL {hours:UInt32} HOUR
      GROUP BY hour
      ORDER BY hour
    SQL
    data = rows.map do |r|
      {
        hour: rfc3339(r["hour"]),
        requests: r["requests"],
        successes: r["successes"],
        failures: r["failures"],
        success_rate: rate(r["successes"], r["requests"]),
        avg_duration_ms: r["avg_duration_ms"],
        total_bytes: r["total_bytes"]
      }
    end
    meta = [ { name: "hour", type: "DateTime" },
             { name: "requests", type: "UInt64" },
             { name: "successes", type: "UInt64" },
             { name: "failures", type: "UInt64" },
             { name: "success_rate", type: "Float64" },
             { name: "avg_duration_ms", type: "Float64" },
             { name: "total_bytes", type: "UInt64" } ]
    render_envelope(meta, data, started)
  end

  def daily_stats
    started = monotonic_now
    rows = ClickhouseClient.instance.query(<<~SQL, params: { days: days_param })
      SELECT
          toDate(timestamp) as date,
          sum(pages_fetched) as requests,
          sumIf(pages_fetched, status_code >= 200 AND status_code < 400) as successes,
          sumIf(pages_fetched, status_code >= 400 OR status_code = 0 OR error != '') as failures,
          avg(duration_ms) as avg_duration_ms,
          sum(content_length) as total_bytes
      FROM request_events
      WHERE timestamp >= now() - INTERVAL {days:UInt32} DAY
      GROUP BY date
      ORDER BY date
    SQL
    data = rows.map do |r|
      {
        date: r["date"],
        requests: r["requests"],
        successes: r["successes"],
        failures: r["failures"],
        success_rate: rate(r["successes"], r["requests"]),
        avg_duration_ms: r["avg_duration_ms"],
        total_bytes: r["total_bytes"]
      }
    end
    meta = [ { name: "date", type: "Date" },
             { name: "requests", type: "UInt64" },
             { name: "successes", type: "UInt64" },
             { name: "failures", type: "UInt64" },
             { name: "success_rate", type: "Float64" },
             { name: "avg_duration_ms", type: "Float64" },
             { name: "total_bytes", type: "UInt64" } ]
    render_envelope(meta, data, started)
  end

  def error_distribution
    started = monotonic_now
    rows = ClickhouseClient.instance.query(<<~SQL, params: { hours: hours_param })
      SELECT status_code, count() as count
      FROM request_events
      WHERE timestamp >= now() - INTERVAL {hours:UInt32} HOUR
          AND (status_code >= 400 OR error != '')
      GROUP BY status_code
      ORDER BY count DESC
    SQL
    total = rows.sum { |r| r["count"] }
    data = rows.map do |r|
      {
        status_code: r["status_code"],
        count: r["count"],
        percentage: rate(r["count"], total)
      }
    end
    meta = [ { name: "status_code", type: "UInt16" },
             { name: "count", type: "UInt64" },
             { name: "percentage", type: "Float64" } ]
    render_envelope(meta, data, started)
  end

  def job_stats
    started = monotonic_now
    job_id = params.require(:job_id)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { job_id: job_id })
      SELECT
          job_id,
          #{REQUEST_AGG},
          sum(content_length) as total_bytes,
          avg(duration_ms) as avg_duration_ms,
          uniqExact(domain) as unique_domains,
          min(timestamp) as started_at,
          max(timestamp) as last_activity_at
      FROM request_events
      WHERE job_id = {job_id:String}
      GROUP BY job_id
    SQL
    data = rows.first(1).map do |r|
      started_at = parse_ch_time(r["started_at"])
      last_activity = parse_ch_time(r["last_activity_at"])
      {
        job_id: r["job_id"],
        total_requests: r["total_requests"],
        successful_requests: r["successful_requests"],
        failed_requests: r["failed_requests"],
        success_rate: rate(r["successful_requests"], r["total_requests"]),
        total_bytes: r["total_bytes"],
        avg_duration_ms: r["avg_duration_ms"],
        unique_domains: r["unique_domains"],
        started_at: offset_dt(started_at),
        last_activity_at: offset_dt(last_activity),
        duration_seconds: (last_activity - started_at).to_i
      }
    end
    meta = [ { name: "job_id", type: "String" },
             { name: "total_requests", type: "UInt64" },
             { name: "successful_requests", type: "UInt64" },
             { name: "failed_requests", type: "UInt64" },
             { name: "success_rate", type: "Float64" },
             { name: "total_bytes", type: "UInt64" },
             { name: "avg_duration_ms", type: "Float64" },
             { name: "unique_domains", type: "UInt64" },
             { name: "started_at", type: "DateTime" },
             { name: "last_activity_at", type: "DateTime" },
             { name: "duration_seconds", type: "Int64" } ]
    render_envelope(meta, data, started)
  end

  def kpis
    started = monotonic_now
    domains = top_domains_rows(hours_param, 10_000)

    total_crawls = domains.sum { |d| d[:total_requests] }
    total_successes = domains.sum { |d| d[:successful_requests] }
    total_bytes = domains.sum { |d| d[:total_bytes] }
    total_response_time = domains.sum { |d| d[:avg_duration_ms] * d[:total_requests] }
    errors_count = domains.sum { |d| d[:failed_requests] }

    data = [ {
      total_crawls: total_crawls,
      total_bytes: total_bytes,
      unique_domains: domains.length,
      success_rate: rate(total_successes, total_crawls),
      avg_duration_ms: total_crawls.positive? ? total_response_time / total_crawls : 0.0,
      errors_count: errors_count
    } ]
    meta = [ { name: "total_crawls", type: "UInt64" },
             { name: "total_bytes", type: "UInt64" },
             { name: "unique_domains", type: "UInt64" },
             { name: "success_rate", type: "Float64" },
             { name: "avg_duration_ms", type: "Float64" },
             { name: "errors_count", type: "UInt64" } ]
    render_envelope(meta, data, started)
  end

  def ai_usage
    started = monotonic_now
    account_id = params[:account_id]
    account_filter = account_id.present? ? "AND account_id = {account_id:String}" : ""
    bind = { hours: hours_param }
    bind[:account_id] = account_id if account_id.present?
    rows = ClickhouseClient.instance.query(<<~SQL, params: bind)
      SELECT model, count() as total_calls,
          sum(prompt_tokens) as total_prompt_tokens,
          sum(completion_tokens) as total_completion_tokens,
          sum(total_tokens) as total_tokens,
          avg(duration_ms) as avg_duration_ms
      FROM ai_usage_events
      WHERE timestamp >= now() - INTERVAL {hours:UInt32} HOUR #{account_filter}
      GROUP BY model ORDER BY total_tokens DESC
    SQL
    data = rows.map do |r|
      {
        model: r["model"],
        total_calls: r["total_calls"],
        total_prompt_tokens: r["total_prompt_tokens"],
        total_completion_tokens: r["total_completion_tokens"],
        total_tokens: r["total_tokens"],
        avg_duration_ms: r["avg_duration_ms"]
      }
    end
    meta = [ { name: "model", type: "String" },
             { name: "total_calls", type: "UInt64" },
             { name: "total_prompt_tokens", type: "UInt64" },
             { name: "total_completion_tokens", type: "UInt64" },
             { name: "total_tokens", type: "UInt64" },
             { name: "avg_duration_ms", type: "Float64" } ]
    render_envelope(meta, data, started)
  end

  def job_timeline
    started = monotonic_now
    job_id = params.require(:job_id)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { job_id: job_id, limit: limit_param(100) })
      SELECT *
      FROM job_events
      WHERE job_id = {job_id:String}
      ORDER BY timestamp DESC
      LIMIT {limit:UInt32}
    SQL
    data = rows.map do |r|
      {
        event_type: r["event_type"],
        job_id: r["job_id"],
        account_id: r["account_id"],
        operation: r["operation"],
        index_uid: r["index_uid"],
        pages_crawled: r["pages_crawled"],
        documents_indexed: r["documents_indexed"],
        errors: r["errors"],
        bytes_downloaded: r["bytes_downloaded"],
        duration_secs: r["duration_secs"],
        error: r["error"],
        timestamp: offset_dt(parse_ch_time(r["timestamp"]))
      }
    end
    # The Rust handler declares only these three columns even though rows
    # carry twelve fields — preserved for compatibility.
    meta = [ { name: "event_type", type: "String" },
             { name: "job_id", type: "String" },
             { name: "timestamp", type: "DateTime" } ]
    render_envelope(meta, data, started)
  end

  def job_event_summary
    started = monotonic_now
    job_id = params.require(:job_id)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { job_id: job_id })
      SELECT
          event_type,
          count() as event_count,
          min(timestamp) as first_seen,
          max(timestamp) as last_seen
      FROM job_events
      WHERE job_id = {job_id:String}
      GROUP BY event_type
      ORDER BY first_seen
    SQL
    data = rows.map do |r|
      {
        event_type: r["event_type"],
        event_count: r["event_count"],
        first_seen: offset_dt(parse_ch_time(r["first_seen"])),
        last_seen: offset_dt(parse_ch_time(r["last_seen"]))
      }
    end
    meta = [ { name: "event_type", type: "String" },
             { name: "event_count", type: "UInt64" },
             { name: "first_seen", type: "DateTime" },
             { name: "last_seen", type: "DateTime" } ]
    render_envelope(meta, data, started)
  end

  def account_usage
    started = monotonic_now
    account_id = params.require(:account_id)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { account_id: account_id, hours: hours_param })
      SELECT
          account_id,
          #{REQUEST_AGG},
          sum(content_length) as total_bytes,
          avg(duration_ms) as avg_duration_ms,
          uniqExact(domain) as unique_domains,
          countIf(js_rendered) as js_renders,
          sum(ai_prompt_tokens) as ai_prompt_tokens,
          sum(ai_completion_tokens) as ai_completion_tokens
      FROM request_events
      WHERE account_id = {account_id:String} AND timestamp >= now() - INTERVAL {hours:UInt32} HOUR
      GROUP BY account_id
    SQL
    r = rows.first || {
      "account_id" => account_id, "total_requests" => 0, "successful_requests" => 0,
      "failed_requests" => 0, "total_bytes" => 0, "avg_duration_ms" => 0.0,
      "unique_domains" => 0, "js_renders" => 0, "ai_prompt_tokens" => 0,
      "ai_completion_tokens" => 0
    }
    data = [ {
      account_id: r["account_id"],
      total_requests: r["total_requests"],
      successful_requests: r["successful_requests"],
      failed_requests: r["failed_requests"],
      total_bytes: r["total_bytes"],
      avg_duration_ms: r["avg_duration_ms"],
      unique_domains: r["unique_domains"],
      js_renders: r["js_renders"],
      ai_prompt_tokens: r["ai_prompt_tokens"],
      ai_completion_tokens: r["ai_completion_tokens"]
    } ]
    meta = [ { name: "account_id", type: "String" },
             { name: "total_requests", type: "UInt64" },
             { name: "successful_requests", type: "UInt64" },
             { name: "failed_requests", type: "UInt64" },
             *USAGE_META_TAIL ]
    render_envelope(meta, data, started)
  end

  def account_daily_usage
    started = monotonic_now
    account_id = params.require(:account_id)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { account_id: account_id, days: days_param })
      SELECT
          toDate(timestamp) as date,
          sum(pages_fetched) as requests,
          sum(content_length) as bytes,
          countIf(js_rendered) as js_renders,
          sum(ai_prompt_tokens) as ai_prompt_tokens,
          sum(ai_completion_tokens) as ai_completion_tokens
      FROM request_events
      WHERE account_id = {account_id:String} AND timestamp >= now() - INTERVAL {days:UInt32} DAY
      GROUP BY date
      ORDER BY date
    SQL
    data = rows.map do |r|
      {
        date: r["date"],
        requests: r["requests"],
        bytes: r["bytes"],
        js_renders: r["js_renders"],
        ai_prompt_tokens: r["ai_prompt_tokens"],
        ai_completion_tokens: r["ai_completion_tokens"]
      }
    end
    meta = [ { name: "date", type: "Date" },
             { name: "requests", type: "UInt64" },
             { name: "bytes", type: "UInt64" },
             { name: "js_renders", type: "UInt64" },
             { name: "ai_prompt_tokens", type: "UInt64" },
             { name: "ai_completion_tokens", type: "UInt64" } ]
    render_envelope(meta, data, started)
  end

  def account_daily_usage_by_operation
    started = monotonic_now
    account_id = params.require(:account_id)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { account_id: account_id, days: days_param })
      SELECT
          toDate(timestamp) as date,
          operation,
          sum(pages_fetched) as requests,
          sum(content_length) as bytes,
          countIf(js_rendered) as js_renders,
          sum(ai_prompt_tokens) as ai_prompt_tokens,
          sum(ai_completion_tokens) as ai_completion_tokens
      FROM request_events
      WHERE account_id = {account_id:String} AND timestamp >= now() - INTERVAL {days:UInt32} DAY
      GROUP BY date, operation
      ORDER BY date, operation
    SQL
    data = rows.map do |r|
      {
        date: r["date"],
        operation: r["operation"],
        requests: r["requests"],
        bytes: r["bytes"],
        js_renders: r["js_renders"],
        ai_prompt_tokens: r["ai_prompt_tokens"],
        ai_completion_tokens: r["ai_completion_tokens"]
      }
    end
    meta = [ { name: "date", type: "Date" },
             { name: "operation", type: "String" },
             { name: "requests", type: "UInt64" },
             { name: "bytes", type: "UInt64" },
             { name: "js_renders", type: "UInt64" },
             { name: "ai_prompt_tokens", type: "UInt64" },
             { name: "ai_completion_tokens", type: "UInt64" } ]
    render_envelope(meta, data, started)
  end

  def api_key_usage
    started = monotonic_now
    account_id = params.require(:account_id)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { account_id: account_id, hours: hours_param })
      SELECT
          api_key_id,
          #{REQUEST_AGG},
          sum(content_length) as total_bytes,
          avg(duration_ms) as avg_duration_ms,
          uniqExact(domain) as unique_domains,
          countIf(js_rendered) as js_renders,
          sum(ai_prompt_tokens) as ai_prompt_tokens,
          sum(ai_completion_tokens) as ai_completion_tokens
      FROM request_events
      WHERE account_id = {account_id:String} AND api_key_id != '' AND timestamp >= now() - INTERVAL {hours:UInt32} HOUR
      GROUP BY api_key_id
      ORDER BY total_requests DESC
    SQL
    data = rows.map do |r|
      {
        api_key_id: r["api_key_id"],
        total_requests: r["total_requests"],
        successful_requests: r["successful_requests"],
        failed_requests: r["failed_requests"],
        total_bytes: r["total_bytes"],
        avg_duration_ms: r["avg_duration_ms"],
        unique_domains: r["unique_domains"],
        js_renders: r["js_renders"],
        ai_prompt_tokens: r["ai_prompt_tokens"],
        ai_completion_tokens: r["ai_completion_tokens"]
      }
    end
    meta = [ { name: "api_key_id", type: "String" },
             { name: "total_requests", type: "UInt64" },
             { name: "successful_requests", type: "UInt64" },
             { name: "failed_requests", type: "UInt64" },
             *USAGE_META_TAIL ]
    render_envelope(meta, data, started)
  end

  private

  def require_clickhouse
    head :not_found unless ClickhouseClient.configured?
  end

  def render_envelope(meta, data, started)
    render json: {
      meta: meta,
      data: data,
      rows: data.length,
      statistics: {
        elapsed: monotonic_now - started,
        rows_read: data.length,
        bytes_read: 0
      }
    }
  end

  def top_domains_rows(hours, limit)
    rows = ClickhouseClient.instance.query(<<~SQL, params: { hours: hours, limit: limit })
      SELECT
          domain,
          #{REQUEST_AGG},
          avg(duration_ms) as avg_duration_ms,
          sum(content_length) as total_bytes
      FROM request_events
      WHERE timestamp >= now() - INTERVAL {hours:UInt32} HOUR
      GROUP BY domain
      ORDER BY total_requests DESC
      LIMIT {limit:UInt32}
    SQL
    rows.map { |r| domain_row(r) }
  end

  def domain_row(r)
    {
      domain: r["domain"],
      total_requests: r["total_requests"],
      successful_requests: r["successful_requests"],
      failed_requests: r["failed_requests"],
      success_rate: rate(r["successful_requests"], r["total_requests"]),
      avg_duration_ms: r["avg_duration_ms"],
      total_bytes: r["total_bytes"]
    }
  end

  def rate(part, total)
    total.to_i.positive? ? part.to_f / total * 100.0 : 0.0
  end

  def hours_param
    (params[:hours].presence || 24).to_i
  end

  def days_param
    (params[:days].presence || 30).to_i
  end

  def limit_param(default)
    (params[:limit].presence || default).to_i
  end

  def monotonic_now
    Process.clock_gettime(Process::CLOCK_MONOTONIC)
  end

  # ClickHouse returns DateTime as "YYYY-MM-DD HH:MM:SS" in UTC.
  def parse_ch_time(value)
    Time.strptime("#{value} UTC", "%Y-%m-%d %H:%M:%S %Z").utc
  end

  def rfc3339(value)
    parse_ch_time(value).strftime("%Y-%m-%dT%H:%M:%SZ")
  end

  # Rust `time::OffsetDateTime` Display: hour without zero padding,
  # ".0" fractional second, "+00:00:00" offset.
  def offset_dt(time)
    time.strftime("%Y-%m-%d %-H:%M:%S.0 +00:00:00")
  end

  PIPES = [
    {
      name: "top_domains",
      description: "Top domains by request count",
      parameters: [
        { name: "hours", type: "integer", required: false, default: "24" },
        { name: "limit", type: "integer", required: false, default: "20" }
      ],
      endpoint: "/analytics/v0/pipes/top_domains.json"
    },
    {
      name: "domain_stats",
      description: "Statistics for a specific domain",
      parameters: [
        { name: "domain", type: "string", required: true, default: nil },
        { name: "hours", type: "integer", required: false, default: "24" }
      ],
      endpoint: "/analytics/v0/pipes/domain_stats.json"
    },
    {
      name: "hourly_stats",
      description: "Hourly crawl statistics",
      parameters: [ { name: "hours", type: "integer", required: false, default: "24" } ],
      endpoint: "/analytics/v0/pipes/hourly_stats.json"
    },
    {
      name: "daily_stats",
      description: "Daily crawl statistics",
      parameters: [ { name: "days", type: "integer", required: false, default: "30" } ],
      endpoint: "/analytics/v0/pipes/daily_stats.json"
    },
    {
      name: "error_distribution",
      description: "Error breakdown by status code",
      parameters: [ { name: "hours", type: "integer", required: false, default: "24" } ],
      endpoint: "/analytics/v0/pipes/error_distribution.json"
    },
    {
      name: "job_stats",
      description: "Statistics for a specific job",
      parameters: [ { name: "job_id", type: "string", required: true, default: nil } ],
      endpoint: "/analytics/v0/pipes/job_stats.json"
    },
    {
      name: "kpis",
      description: "Key performance indicators summary",
      parameters: [ { name: "hours", type: "integer", required: false, default: "24" } ],
      endpoint: "/analytics/v0/pipes/kpis.json"
    },
    {
      name: "ai_usage",
      description: "AI/LLM token usage per model",
      parameters: [
        { name: "hours", type: "integer", required: false, default: "24" },
        { name: "account_id", type: "string", required: false, default: nil }
      ],
      endpoint: "/analytics/v0/pipes/ai_usage.json"
    },
    {
      name: "job_timeline",
      description: "Job lifecycle events (started, completed, failed)",
      parameters: [
        { name: "job_id", type: "string", required: true, default: nil },
        { name: "limit", type: "integer", required: false, default: "100" }
      ],
      endpoint: "/analytics/v0/pipes/job_timeline.json"
    },
    {
      name: "job_event_summary",
      description: "Event type counts for a specific job",
      parameters: [ { name: "job_id", type: "string", required: true, default: nil } ],
      endpoint: "/analytics/v0/pipes/job_event_summary.json"
    },
    {
      name: "account_usage",
      description: "Account usage summary (requests, bandwidth, JS renders, AI tokens)",
      parameters: [
        { name: "account_id", type: "string", required: true, default: nil },
        { name: "hours", type: "integer", required: false, default: "24" }
      ],
      endpoint: "/analytics/v0/pipes/account_usage.json"
    },
    {
      name: "account_daily_usage",
      description: "Daily usage breakdown for an account",
      parameters: [
        { name: "account_id", type: "string", required: true, default: nil },
        { name: "days", type: "integer", required: false, default: "30" }
      ],
      endpoint: "/analytics/v0/pipes/account_daily_usage.json"
    },
    {
      name: "account_daily_usage_by_operation",
      description: "Daily usage breakdown per operation (scrape/map/crawl) for an account",
      parameters: [
        { name: "account_id", type: "string", required: true, default: nil },
        { name: "days", type: "integer", required: false, default: "30" }
      ],
      endpoint: "/analytics/v0/pipes/account_daily_usage_by_operation.json"
    },
    {
      name: "api_key_usage",
      description: "Per-API-key usage breakdown for an account",
      parameters: [
        { name: "account_id", type: "string", required: true, default: nil },
        { name: "hours", type: "integer", required: false, default: "24" }
      ],
      endpoint: "/analytics/v0/pipes/api_key_usage.json"
    }
  ].freeze
end
