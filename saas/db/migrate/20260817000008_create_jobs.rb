class CreateJobs < ActiveRecord::Migration[8.1]
  def change
    # Owned and written by the Rust engine; Rails reads it (read-only model).
    create_table :jobs, id: :text, primary_key: :job_id do |t|
      t.text :status, null: false, default: "pending"
      t.text :index_uid, null: false
      t.uuid :account_id
      t.uuid :api_key_id
      t.bigint :pages_crawled, null: false, default: 0
      t.bigint :pages_indexed, null: false, default: 0
      t.bigint :documents_sent, null: false, default: 0
      t.bigint :errors, null: false, default: 0
      t.bigint :bytes_downloaded, null: false, default: 0
      t.timestamptz :started_at
      t.timestamptz :completed_at
      t.float :crawl_rate, null: false, default: 0.0
      t.bigint :eta_seconds
      t.text :error_message
      t.jsonb :start_urls, null: false, default: []
      t.bigint :max_pages
      t.jsonb :config
      t.text :swap_temp_index
      t.text :swap_meilisearch_url
      t.text :swap_meilisearch_api_key

      t.timestamps default: -> { "now()" }

      t.check_constraint "status IN ('pending', 'running', 'completed', 'failed', 'cancelled', 'paused')",
                         name: "jobs_status_check"
    end

    add_foreign_key :jobs, :accounts, on_delete: :nullify
    add_foreign_key :jobs, :api_keys, on_delete: :nullify
    add_index :jobs, :account_id
    add_index :jobs, :status
    add_index :jobs, :created_at, order: { created_at: :desc }
    add_index :jobs, :job_id, where: "status IN ('pending', 'running', 'paused')", name: "index_jobs_on_job_id_active"

    # The engine updates job rows with plain SQL and relies on the database to
    # maintain updated_at (Rails-owned tables get it from ActiveRecord).
    reversible do |dir|
      dir.up do
        execute <<~SQL
          CREATE OR REPLACE FUNCTION update_updated_at()
          RETURNS TRIGGER AS $$
          BEGIN
              NEW.updated_at = now();
              RETURN NEW;
          END;
          $$ LANGUAGE plpgsql;

          CREATE TRIGGER trg_jobs_updated_at
              BEFORE UPDATE ON jobs
              FOR EACH ROW EXECUTE FUNCTION update_updated_at();
        SQL
      end
      dir.down do
        execute "DROP TRIGGER IF EXISTS trg_jobs_updated_at ON jobs"
        execute "DROP FUNCTION IF EXISTS update_updated_at()"
      end
    end
  end
end
