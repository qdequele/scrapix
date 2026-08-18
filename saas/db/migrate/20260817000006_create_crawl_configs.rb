class CreateCrawlConfigs < ActiveRecord::Migration[8.1]
  def change
    create_table :crawl_configs, id: :uuid do |t|
      t.references :account, type: :uuid, null: false, foreign_key: { on_delete: :cascade }
      t.text :name, null: false
      t.text :description
      t.jsonb :config, null: false
      t.text :cron_expression
      t.boolean :cron_enabled, null: false, default: false
      t.timestamptz :last_run_at
      t.timestamptz :next_run_at
      t.text :last_job_id

      t.timestamps default: -> { "now()" }

      t.index [ :account_id, :name ], unique: true
      # The engine's cron scheduler polls due configs on this partial index.
      t.index :next_run_at,
              where: "cron_enabled = true AND cron_expression IS NOT NULL",
              name: "index_crawl_configs_on_next_run_at_due"
    end
  end
end
