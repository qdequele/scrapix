CREATE TABLE lab_events (
    id uuid PRIMARY KEY,
    type text NOT NULL,
    account_id uuid NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz
);
CREATE INDEX lab_events_due_idx ON lab_events (next_attempt_at) WHERE delivered_at IS NULL;
CREATE INDEX lab_events_delivered_idx ON lab_events (delivered_at) WHERE delivered_at IS NOT NULL;
