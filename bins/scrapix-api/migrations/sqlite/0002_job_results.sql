CREATE TABLE job_results (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id TEXT NOT NULL REFERENCES jobs (job_id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL DEFAULT 'page' CHECK (kind IN ('page','extract')),
    url TEXT,
    success INTEGER NOT NULL DEFAULT 1,
    payload TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (job_id, seq)
);
