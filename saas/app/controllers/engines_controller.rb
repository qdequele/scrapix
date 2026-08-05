# Meilisearch engine registry — port of bins/scrapix-api/src/engines.rs.
# CRUD over meilisearch_engines plus proxying of index listing and search to
# the registered Meilisearch instance.
class EnginesController < ApplicationController
  before_action :authenticate_api_key_or_session!

  def create
    account_id = resolve_account_id!
    name = params[:name].to_s
    url = params[:url].to_s
    api_error!("Name is required", "validation_error") if name.strip.empty?
    api_error!("URL is required", "validation_error") if url.strip.empty?

    is_default = ActiveModel::Type::Boolean.new.cast(params[:is_default]) || false
    record = MeilisearchEngine.transaction do
      if is_default
        MeilisearchEngine.where(account_id: account_id, is_default: true).update_all(is_default: false)
      end
      MeilisearchEngine.create!(
        account_id: account_id,
        name: name.strip,
        url: url.strip,
        api_key: params[:api_key].to_s,
        is_default: is_default
      )
    end
    render json: serialize(record), status: :created
  rescue ActiveRecord::RecordNotUnique, ActiveRecord::RecordInvalid => e
    handle_conflict(e)
  end

  def index
    account_id = resolve_account_id!
    records = MeilisearchEngine.where(account_id: account_id)
                               .order(is_default: :desc, created_at: :desc)
    render json: records.map { |r| serialize(r) }
  end

  def show
    render json: serialize(find_engine!)
  end

  def update
    record = find_engine!

    new_name = params.key?(:name) ? params[:name].to_s : record.name
    api_error!("Name cannot be empty", "validation_error") if new_name.strip.empty?
    new_url = params.key?(:url) ? params[:url].to_s : record.url
    api_error!("URL cannot be empty", "validation_error") if new_url.strip.empty?
    new_api_key = params.key?(:api_key) ? params[:api_key].to_s : record.api_key

    record.update!(name: new_name.strip, url: new_url.strip, api_key: new_api_key)
    render json: serialize(record)
  rescue ActiveRecord::RecordNotUnique, ActiveRecord::RecordInvalid => e
    handle_conflict(e)
  end

  def destroy
    find_engine!.delete
    head :no_content
  end

  def set_default
    record = find_engine!
    MeilisearchEngine.transaction do
      MeilisearchEngine.where(account_id: record.account_id, is_default: true)
                       .update_all(is_default: false)
      record.update_columns(is_default: true)
    end
    render json: serialize(record.reload)
  end

  def indexes
    record = find_engine!
    body = MeilisearchProxy.get(record, "/indexes?limit=100")
    indexes = (body["results"] || []).map do |idx|
      {
        uid: idx["uid"],
        primaryKey: idx["primaryKey"],
        createdAt: idx["createdAt"],
        updatedAt: idx["updatedAt"]
      }
    end
    render json: indexes
  end

  def search
    record = find_engine!
    index_uid = params[:index_uid]
    payload = request.request_parameters.except("id", "index_uid")
    result = MeilisearchProxy.post(record, "/indexes/#{index_uid}/search", payload)
    render json: result
  end

  private

  def find_engine!
    account_id = resolve_account_id!
    engine_id = params[:id]
    api_error!("Invalid engine ID", "validation_error") unless uuid?(engine_id)
    MeilisearchEngine.find_by(id: engine_id, account_id: account_id) ||
      api_error!("Engine not found", "not_found")
  end

  def handle_conflict(error)
    if error.is_a?(ActiveRecord::RecordNotUnique) ||
       error.message.include?("has already been taken")
      api_error!("An engine with this name already exists", "conflict")
    else
      api_error!("Failed to save engine: #{error.message}", "internal_error")
    end
  end

  def uuid?(value)
    value.to_s.match?(/\A[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z/i)
  end

  def serialize(record)
    {
      id: record.id,
      account_id: record.account_id,
      name: record.name,
      url: record.url,
      api_key: record.api_key,
      is_default: record.is_default,
      created_at: rfc3339_auto(record.created_at),
      updated_at: rfc3339_auto(record.updated_at)
    }
  end
end
