# Saved crawl configs — port of bins/scrapix-api/src/configs.rs.
#
# The cron *scheduler* stays in the Rust engine (it polls crawl_configs
# directly); this controller owns the CRUD + trigger surface. Trigger submits
# the stored config to the engine's POST /crawl, forwarding the caller's
# credentials, then stamps last_run_at/last_job_id like the Rust handler.
class ConfigsController < ApplicationController
  before_action :authenticate_api_key_or_session!

  def create
    account_id = resolve_account_id!
    name = params[:name].to_s
    api_error!("Name is required", "validation_error") if name.strip.empty?

    config = require_config_param
    cron_expression = params[:cron_expression].presence
    cron_enabled = ActiveModel::Type::Boolean.new.cast(params[:cron_enabled]) || false
    next_run_at = validate_cron(cron_expression, cron_enabled)

    record = CrawlConfig.create!(
      account_id: account_id,
      name: name.strip,
      description: params[:description].presence,
      config: CrawlConfigNormalizer.normalize(config),
      cron_expression: cron_expression,
      cron_enabled: cron_enabled,
      next_run_at: next_run_at
    )
    render json: serialize(record), status: :created
  rescue ActiveRecord::RecordNotUnique, ActiveRecord::RecordInvalid => e
    handle_conflict(e, "A config with this name already exists")
  end

  def index
    account_id = resolve_account_id!
    limit = (params[:limit].presence || 50).to_i
    offset = (params[:offset].presence || 0).to_i
    records = CrawlConfig.where(account_id: account_id)
                         .order(created_at: :desc)
                         .limit(limit).offset(offset)
    render json: records.map { |r| serialize(r) }
  end

  def show
    render json: serialize(find_config!)
  end

  def update
    record = find_config!

    new_name = params.key?(:name) ? params[:name].to_s : record.name
    api_error!("Name cannot be empty", "validation_error") if new_name.strip.empty?

    new_description = params.key?(:description) ? params[:description].presence : record.description
    new_config =
      if params.key?(:config)
        CrawlConfigNormalizer.normalize(require_config_param)
      else
        record.config
      end
    new_cron_expression =
      params.key?(:cron_expression) ? params[:cron_expression].presence : record.cron_expression
    new_cron_enabled =
      params.key?(:cron_enabled) ? ActiveModel::Type::Boolean.new.cast(params[:cron_enabled]) : record.cron_enabled

    next_run_at =
      if new_cron_enabled && new_cron_expression
        compute_next_run(new_cron_expression)
      end

    record.update!(
      name: new_name.strip,
      description: new_description,
      config: new_config,
      cron_expression: new_cron_expression,
      cron_enabled: new_cron_enabled,
      next_run_at: next_run_at
    )
    render json: serialize(record)
  rescue ActiveRecord::RecordNotUnique, ActiveRecord::RecordInvalid => e
    handle_conflict(e, "A config with this name already exists")
  end

  def destroy
    find_config!.delete
    head :no_content
  end

  def trigger
    record = find_config!
    response = ScrapixEngine.create_crawl(record.config, forwarded_credentials)
    record.update_columns(last_run_at: Time.current, last_job_id: response["job_id"])

    render json: {
      job_id: response["job_id"],
      config_id: record.id,
      message: "Crawl triggered successfully"
    }
  rescue ScrapixEngine::EngineError => e
    # Propagate the engine's ApiError body (e.g. insufficient_credits) as-is.
    render json: e.body.presence || { error: "Crawl engine unavailable", code: "service_unavailable" },
           status: e.status
  end

  private

  def find_config!
    account_id = resolve_account_id!
    config_id = params[:id]
    api_error!("Invalid config ID", "validation_error") unless uuid?(config_id)
    CrawlConfig.find_by(id: config_id, account_id: account_id) ||
      api_error!("Config not found", "not_found")
  end

  def require_config_param
    config = params[:config]
    config = config.to_unsafe_h if config.is_a?(ActionController::Parameters)
    unless config.is_a?(Hash) && config["start_urls"].is_a?(Array) && config["index_uid"].is_a?(String)
      api_error!("Invalid config: start_urls and index_uid are required", "validation_error")
    end
    config
  end

  def validate_cron(expression, enabled)
    return nil if expression.nil?

    next_run = compute_next_run(expression)
    enabled ? next_run : nil
  end

  def compute_next_run(expression)
    cron = Fugit.parse_cron(expression)
    api_error!("Invalid cron expression: #{expression}", "validation_error") if cron.nil?
    cron.next_time(Time.current).to_t.utc
  end

  def handle_conflict(error, message)
    if error.is_a?(ActiveRecord::RecordNotUnique) ||
       error.message.include?("has already been taken")
      api_error!(message, "conflict")
    else
      api_error!("Failed to save config: #{error.message}", "internal_error")
    end
  end

  def uuid?(value)
    value.to_s.match?(/\A[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z/i)
  end

  def forwarded_credentials
    {
      api_key: request.headers["X-API-Key"],
      bearer: request.headers["Authorization"],
      session_cookie: cookies["scrapix_session"],
      account_id: request.headers["X-Account-Id"]
    }
  end

  def serialize(record)
    {
      id: record.id,
      account_id: record.account_id,
      name: record.name,
      description: record.description,
      config: record.config,
      cron_expression: record.cron_expression,
      cron_enabled: record.cron_enabled,
      last_run_at: record.last_run_at && rfc3339_auto(record.last_run_at),
      next_run_at: record.next_run_at && rfc3339_auto(record.next_run_at),
      last_job_id: record.last_job_id,
      created_at: rfc3339_auto(record.created_at),
      updated_at: rfc3339_auto(record.updated_at)
    }
  end
end
