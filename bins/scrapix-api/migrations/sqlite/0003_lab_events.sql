CREATE TABLE lab_events (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    account_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    delivered_at TEXT
);
CREATE INDEX lab_events_due_idx ON lab_events (next_attempt_at) WHERE delivered_at IS NULL;
CREATE INDEX lab_events_delivered_idx ON lab_events (delivered_at) WHERE delivered_at IS NOT NULL;
