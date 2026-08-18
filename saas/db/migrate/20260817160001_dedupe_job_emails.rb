# A completed crawl was queueing two identical job_completed emails (the
# engine's event processor and idle detector both fire). Enforce one
# notification per (type, job) at the database level; the engine inserts
# with ON CONFLICT DO NOTHING.
class DedupeJobEmails < ActiveRecord::Migration[8.1]
  def up
    # Collapse any existing duplicates before adding the unique index.
    execute <<~SQL
      DELETE FROM scheduled_emails a
      USING scheduled_emails b
      WHERE a.email_type IN ('job_completed', 'job_failed')
        AND a.email_type = b.email_type
        AND a.payload->>'job_id' = b.payload->>'job_id'
        AND a.created_at > b.created_at;
    SQL
    execute <<~SQL
      CREATE UNIQUE INDEX index_scheduled_emails_job_dedupe
      ON scheduled_emails (email_type, (payload->>'job_id'))
      WHERE email_type IN ('job_completed', 'job_failed');
    SQL
  end

  def down
    execute "DROP INDEX index_scheduled_emails_job_dedupe"
  end
end
