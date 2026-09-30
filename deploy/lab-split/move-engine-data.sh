#!/usr/bin/env bash
# deploy/lab-split/move-engine-data.sh — one-time copy of the engine's tables
# from the Lab (Rails) database into the engine's own database (Lab split
# spec §4.2). Run with the engine STOPPED (drain or cancel jobs first), after
# starting the new engine once against the empty engine database so it has
# migrated it (docs/operations/crawl-engine-rollout.mdx, "Lab split phase 1").
#
# Usage: move-engine-data.sh <lab-db-url> <engine-db-url>
#
# Copies jobs, job_results and lab_events (undelivered events included, so they
# are delivered by the new engine). Leaves the Lab's copies in place: they are
# the rollback path, and the Lab ignores them. Refuses to run twice (the engine
# tables must be empty) and compares row counts afterwards.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <lab-db-url> <engine-db-url>" >&2
  exit 2
fi
LAB_DB="$1"; ENGINE_DB="$2"
TABLES=(jobs job_results lab_events)

if [ "$LAB_DB" = "$ENGINE_DB" ]; then
  echo "the Lab and engine database URLs are identical: the engine needs its own database" >&2
  exit 2
fi

for t in "${TABLES[@]}"; do
  [ "$(psql "$LAB_DB" -tAc "SELECT to_regclass('$t') IS NOT NULL")" = "t" ] \
    || { echo "missing table $t in the Lab database: is this the right <lab-db-url>?"; exit 1; }
  [ "$(psql "$ENGINE_DB" -tAc "SELECT to_regclass('$t') IS NOT NULL")" = "t" ] \
    || { echo "missing table $t in the engine database: start the new engine once first"; exit 1; }
  n=$(psql "$ENGINE_DB" -tAc "SELECT count(*) FROM $t")
  [ "$n" = "0" ] || { echo "engine table $t already has $n rows: refusing to copy twice"; exit 1; }
done

active=$(psql "$LAB_DB" -tAc "SELECT count(*) FROM jobs WHERE status IN ('pending','running','paused')")
if [ "$active" != "0" ]; then
  echo "warning: $active job(s) are pending/running/paused in the Lab database. They will be copied as-is;" >&2
  echo "         a running one ends FailStalled (billed) once the stall timeout passes. Drain first next time." >&2
fi

args=()
for t in "${TABLES[@]}"; do args+=(-t "$t"); done
# One transaction: a failure leaves the engine tables empty, so the script can
# simply be re-run. The Rails jobs timestamps are `timestamp` (UTC values) and
# the engine's are `timestamptz`, so pin the session zone to UTC for the copy.
{
  echo "SET timezone = 'UTC';"
  pg_dump --data-only --no-owner --no-privileges "${args[@]}" "$LAB_DB"
} | psql -v ON_ERROR_STOP=1 -1 -q "$ENGINE_DB" >/dev/null

# job_results.id is a bigserial: make sure new rows continue after the copied ids.
psql -v ON_ERROR_STOP=1 -q "$ENGINE_DB" -tAc \
  "SELECT setval(pg_get_serial_sequence('job_results','id'), COALESCE(max(id), 0) + 1, false) FROM job_results" >/dev/null

for t in "${TABLES[@]}"; do
  a=$(psql "$LAB_DB" -tAc "SELECT count(*) FROM $t")
  b=$(psql "$ENGINE_DB" -tAc "SELECT count(*) FROM $t")
  echo "$t: lab=$a engine=$b"
  [ "$a" = "$b" ] || { echo "row count mismatch for $t"; exit 1; }
done
echo "Done. Undelivered lab events moved: $(psql "$ENGINE_DB" -tAc "SELECT count(*) FROM lab_events WHERE delivered_at IS NULL")"
