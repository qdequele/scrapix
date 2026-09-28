-- Engine-owned `job_results` table for standalone mode (mirrors the
-- Rails-owned shape in saas/db/structure.sql's `CREATE TABLE
-- public.job_results`).
CREATE TABLE job_results (
    id bigserial PRIMARY KEY,
    job_id text NOT NULL REFERENCES jobs (job_id) ON DELETE CASCADE,
    seq integer NOT NULL,
    kind text NOT NULL DEFAULT 'page' CHECK (kind IN ('page','extract')),
    url text,
    success boolean NOT NULL DEFAULT true,
    payload jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (job_id, seq)
);
