# Exact per-job work accounting (crawl-engine-fixes R5). The Rust engine
# persists its JobAccounting snapshot here with the periodic counter flush
# and restores it for running jobs at startup; Rails never reads it.
class AddAccountingToJobs < ActiveRecord::Migration[8.1]
  def change
    add_column :jobs, :accounting, :jsonb, null: false, default: {}
  end
end
