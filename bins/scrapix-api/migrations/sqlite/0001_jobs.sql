CREATE TABLE jobs (
    job_id TEXT PRIMARY KEY NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','running','completed','failed','cancelled','paused')),
    index_uid TEXT NOT NULL,
    account_id TEXT,
    api_key_id TEXT,
    pages_crawled INTEGER NOT NULL DEFAULT 0,
    pages_indexed INTEGER NOT NULL DEFAULT 0,
    documents_sent INTEGER NOT NULL DEFAULT 0,
    errors INTEGER NOT NULL DEFAULT 0,
    bytes_downloaded INTEGER NOT NULL DEFAULT 0,
    started_at TEXT,
    completed_at TEXT,
    crawl_rate REAL NOT NULL DEFAULT 0,
    eta_seconds INTEGER,
    error_message TEXT,
    start_urls TEXT NOT NULL DEFAULT '[]',
    max_pages INTEGER,
    config TEXT,
    swap_temp_index TEXT,
    swap_meilisearch_url TEXT,
    swap_meilisearch_api_key TEXT,
    accounting TEXT NOT NULL DEFAULT '{}',
    -- millisecond precision so same-second jobs still sort newest first
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX jobs_account_idx ON jobs (account_id);
CREATE INDEX jobs_status_idx ON jobs (status);
CREATE INDEX jobs_created_idx ON jobs (created_at DESC);
CREATE TRIGGER jobs_updated_at AFTER UPDATE ON jobs FOR EACH ROW
BEGIN
    UPDATE jobs SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE job_id = NEW.job_id;
END;
