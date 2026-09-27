# Results of the jobs the Rust engine runs itself (SCR-74 batch scrape,
# SCR-73 extract), served by the engine's GET /job/{id}/results. The engine
# owns the write path; Rails never reads it.
#
# - kind "page": one row per URL processed, seq >= 1 in completion order
#   (append-only, so keyset paging on seq is stable while the job runs);
#   payload is the /scrape-shaped result item.
# - kind "extract": the extraction output of an extract job, at seq 0.
#
# Crawl jobs' results stay in their Meilisearch index.
class CreateJobResults < ActiveRecord::Migration[8.1]
  def change
    create_table :job_results do |t|
      t.text :job_id, null: false
      t.integer :seq, null: false
      t.text :kind, null: false, default: "page"
      t.text :url
      t.boolean :success, null: false, default: true
      t.jsonb :payload, null: false, default: {}
      t.timestamptz :created_at, null: false, default: -> { "now()" }

      t.index [ :job_id, :seq ], unique: true
      t.check_constraint "kind IN ('page', 'extract')", name: "job_results_kind_check"
    end
    add_foreign_key :job_results, :jobs, column: :job_id, primary_key: :job_id, on_delete: :cascade
  end
end
