-- Engine-owned `jobs` table for standalone mode (mirrors the Rails-owned
-- shape in saas/db/structure.sql's `CREATE TABLE public.jobs`, minus the
-- account_id/api_key_id foreign keys, which only exist in the SaaS schema).
CREATE TABLE jobs (
    job_id text PRIMARY KEY,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','running','completed','failed','cancelled','paused')),
    index_uid text NOT NULL,
    account_id uuid,
    api_key_id uuid,
    pages_crawled bigint NOT NULL DEFAULT 0,
    pages_indexed bigint NOT NULL DEFAULT 0,
    documents_sent bigint NOT NULL DEFAULT 0,
    errors bigint NOT NULL DEFAULT 0,
    bytes_downloaded bigint NOT NULL DEFAULT 0,
    started_at timestamptz,
    completed_at timestamptz,
    crawl_rate double precision NOT NULL DEFAULT 0,
    eta_seconds bigint,
    error_message text,
    start_urls jsonb NOT NULL DEFAULT '[]',
    max_pages bigint,
    config jsonb,
    swap_temp_index text,
    swap_meilisearch_url text,
    swap_meilisearch_api_key text,
    accounting jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_account_id_idx ON jobs (account_id);
CREATE INDEX jobs_status_idx ON jobs (status);
CREATE INDEX jobs_created_at_idx ON jobs (created_at DESC);
CREATE INDEX jobs_active_idx ON jobs (job_id) WHERE status IN ('pending','running','paused');
CREATE FUNCTION scrapix_jobs_touch_updated_at() RETURNS trigger AS $$
BEGIN NEW.updated_at = now(); RETURN NEW; END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER trg_jobs_updated_at BEFORE UPDATE ON jobs
    FOR EACH ROW EXECUTE FUNCTION scrapix_jobs_touch_updated_at();
