# Scrapix: Lab platform contract v2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

Step 0: copy the spec and this plan from the advisor worktree paths above into this worktree's docs/superpowers/ before starting:

```bash
cd /Users/quentindequelen/Projects/Meilisearch/_side_projects/scrapix/.claude/worktrees/lab-contract-v2
mkdir -p docs/superpowers/specs docs/superpowers/plans
cp /Users/quentindequelen/Projects/Meilisearch/_side_projects/meilisearch-lab/.claude/worktrees/advisor-838e2a/docs/superpowers/specs/2026-10-08-lab-platform-contract-v2.md docs/superpowers/specs/
cp /Users/quentindequelen/Projects/Meilisearch/_side_projects/meilisearch-lab/.claude/worktrees/advisor-838e2a/docs/superpowers/plans/2026-10-08-scrapix-lab-contract-v2.md docs/superpowers/plans/
git add docs/superpowers && git commit -m "docs: add the Lab platform contract v2 spec and plan"
```

**Goal:** Move the Scrapix engine to the Lab platform contract v2: per-instance credentials (`LAB_INSTANCE_ID` / `LAB_INSTANCE_SECRET`), the Lab-owned events schema with raw units instead of credits, plan limits served by the Lab, and the tenant-isolation, CI, deploy, docs and dead-code fixes found in the audit.

**Architecture:** The engine keeps its outbox/sink/client structure (`lab_events.rs`, `lab_sink.rs`, `lab_client.rs`) and changes what travels over it: every engine-to-Lab request carries `X-Lab-Instance-Id` plus the instance secret (Bearer for service calls, timestamped HMAC for event batches), `usage.recorded` carries `units` + `provider_cost_micro_usd` and no `credits`, and the introspect/account answers carry `limits` that replace the tier constants. A standalone engine never talks to a Lab: `LAB_INSTANCE_*` is required in hosted mode and refused in standalone mode. The pre-work pricing and tier code (`scrapix-billing`, `scrapix-core::billing`) is deleted.

**Tech Stack:** Rust 2021 (workspace at `rust-version = "1.88"`), axum 0.8, reqwest 0.12, sqlx 0.8 (SQLite + Postgres), hmac/sha2, jsonschema 0.45 (tests), wiremock (tests), clap 4.

**Spec:** `docs/superpowers/specs/2026-10-08-lab-platform-contract-v2.md`

All paths below are relative to the Scrapix worktree `/Users/quentindequelen/Projects/Meilisearch/_side_projects/scrapix/.claude/worktrees/lab-contract-v2` (branch `qdequele/lab-contract-v2`). Line numbers are from that worktree at `f2ab8d2`; re-check them with `grep -n` before editing, since earlier tasks shift later lines.

## Global Constraints

- Pre-commit, every task (repo `CLAUDE.md`): `cargo fmt && cargo check && cargo clippy` must pass with no errors. CI runs clippy with `RUSTFLAGS: -Dwarnings`, so an unused function is a build failure: delete what you stop calling.
- Never edit `contracts/vendor/` by hand except in Task 1, which vendors the new Lab-owned file (the Lab has not published it yet); `contracts/check-drift.sh --fix` is the only other writer.
- `contracts/openapi.json` is hand-frozen; after editing it run `just sdk-generate` and commit the generated SDK files (CI `just sdk-check` fails otherwise). `contracts/openapi.engine.json` is a snapshot: regenerate with `UPDATE_OPENAPI_SNAPSHOT=1 cargo test -p scrapix-api --test openapi_snapshot` only when the engine's routes or schemas change.
- `bins/scrapix-api/tests/lab_boundary.rs` fails the build on any SQL naming a Lab table (`accounts`, `api_keys`, `transactions`, `lab_events_received`, ...). The engine's own `lab_events` table is fine.
- Engine configuration names are the spec's, byte for byte: `LAB_URL`, `LAB_INSTANCE_ID` (uuid), `LAB_INSTANCE_SECRET` (64 hex chars). Headers: `X-Lab-Instance-Id`, `X-Lab-Timestamp` (unix seconds, integer), `X-Lab-Signature: sha256=<hex HMAC-SHA256(LAB_INSTANCE_SECRET, "<X-Lab-Timestamp>.<raw body>")>`. Service calls: `Authorization: Bearer <LAB_INSTANCE_SECRET>`.
- Event batches: at most 500 events per `POST /internal/events`; response `200 {"accepted": [ids]}`; an event never acknowledged for 24 h is dropped with an error log (spec §3.4).
- `usage.recorded.data`: `operation` (string), `units` (object of integer >= 0), `provider_cost_micro_usd` (integer >= 0, default 0), `description`, `job_id`. Scrapix unit names: `pages_http`, `pages_browser`, `requests`, `documents`, `bytes_out` (extra names are allowed and ignored by Lab pricing). No `credits`.
- Decision A: engines are hosted by Meilisearch only; every engine usage is billed. There is no customer-run Scrapix and no "lab-connected standalone": a standalone engine has no Lab at all. Decision B: no price table and no plan constants in the engine; limits come from `limits { concurrent_jobs, rate_limit_rpm, max_depth, js_rendering }` in the introspect/account responses.
- `LAB_SERVICE_TOKEN` stays, unchanged in name and semantics, for the inbound path only: `Authorization: Bearer <LAB_SERVICE_TOKEN>` + `X-Scrapix-Account-Id` from the Lab's saved-config cron (`crates/scrapix-auth` types, `bins/scrapix-api/src/auth/middleware.rs:18,120-145`). The engine never presents it to the Lab any more.
- `LAB_EVENTS_SECRET` is no longer read for signing; when set, the engine warns at boot and ignores it.
- Commits: conventional messages (`feat(...)`, `fix(...)`, `chore(...)`, `docs(...)`), one per task step marked "Commit". No `Co-Authored-By` lines.

## Review Focus

1. A Lab that still answers v1 (no `limits` in introspect/account, no `GET /internal/instances/me`): the engine must refuse to start with a message naming the missing endpoint, not run unlimited. Pinned in Task 3 (`a_404_on_instances_me_aborts_startup`) and Task 4 (`missing_limits_skips_enforcement_with_one_warning`).
2. A hosted engine whose `LAB_INSTANCE_SECRET` is revoked after boot: every batch gets 401; the outbox must keep retrying and, after 24 h, drop with an error log rather than grow forever. Pinned in Task 3 (`events_older_than_24h_are_dropped_with_an_error`).
3. Clock skew: the Lab rejects a timestamp more than 300 s off. The engine must sign with the current time at send time, never a cached timestamp. Pinned in Task 3 (`signature_covers_timestamp_dot_body`).
4. An operator who sets `LAB_INSTANCE_ID` / `LAB_INSTANCE_SECRET` on a standalone engine (copy-pasted hosted env): the engine must refuse to start with a message naming the variables, never silently ignore them and run unbilled. Pinned in Task 2 (`standalone_refuses_instance_credentials`).
5. A hosted tenant whose account has no Meilisearch target: `/crawl` must answer 400 with the Lab's "Add one in Settings" message and never write into the operator's `MEILISEARCH_URL`; `/job/{id}/results` for such a job must be 404, never the operator's index. Pinned in Task 5 (`hosted_resolver_never_falls_back_to_the_operator_server`, `results_target_is_only_the_jobs_own`).

## Out of scope (stated for the executor)

- API multi-replica job state (in-memory `jobs` map + a shared Kafka consumer group): spec §10.
- ClickHouse analytics ownership (`analytics_pipes.rs` vs the Lab's Rails copy): unchanged.
- Hosted provisioning UI, Helm charts, Lumen/glutony engines: Lab-side and other repos.
- The CLI's other Lab-only commands (`configs`/`config`, `engines`/`engine`, `api-keys`/`api-key`, `billing`, `whoami`): Task 7 removes only the OAuth browser login and `team`, as the brief names; the rest keep working against the hosted platform URL and are listed here so the owner can decide separately.

## File map

| File | Responsibility after this plan |
|------|-------------------------------|
| `contracts/vendor/lab/lab-events.schema.json` | Vendored Lab-owned events schema v2 (byte copy once the Lab publishes it) |
| `contracts/check-drift.sh` | Drift check for both vendored Lab files |
| `bins/scrapix-api/src/lab_events.rs` | `LabEvent` (units only), outbox trait + Pg/SQLite/Memory impls (`abandon` added, credits sum removed) |
| `bins/scrapix-api/src/lab_sink.rs` | Delivery with instance headers, timestamped HMAC, 500-batches, 24 h drop |
| `bins/scrapix-api/src/lab_client.rs` | `LabClient` with instance credentials, `instances_me`, `limits` in answers, balance snapshot without local usage accounting |
| `bins/scrapix-api/src/settings.rs` | `LAB_INSTANCE_*` required in hosted mode, refused in standalone mode |
| `bins/scrapix-api/src/billing.rs` | `BillingError`, the amount-less balance pre-check |
| `bins/scrapix-api/src/engine_jobs.rs` | `preflight` with plan limits (`PlanCheck`, `enforce_limits`) |
| `bins/scrapix-api/src/meili.rs` | Hosted resolver without the operator fallback |
| `bins/scrapix-api/src/results.rs` | Results read only from the job's own target |
| `crates/scrapix-auth/src/types.rs` | `AuthenticatedAccount` (+ `limits`), `Limits` |
| `crates/scrapix-billing/` | Deleted (Task 4) |
| `crates/scrapix-core/src/billing.rs` | Deleted (Task 4) |
| `deploy/kubernetes/base/config/*`, `deploy/kubernetes/README.md` | Hosted manifests with `LAB_INSTANCE_*` secrets |
| `LICENSE`, `Cargo.toml`, `README.md`, `docs/**` | Audit fixes |

---

### Task 1: Lab-owned events schema v2, units-only `usage.recorded`

**Files:**
- Create: `contracts/vendor/lab/lab-events.schema.json`
- Delete: `contracts/lab-events.schema.json`
- Modify: `contracts/check-drift.sh:9` (FILES), `contracts/README.md`
- Modify: `bins/scrapix-api/src/lab_events.rs` (all of `usage_data`, `LabEvent::usage`, `crawl_final_usage`, `usage_credits`, `LabOutbox::undelivered_usage_credits` + its 3 impls, `Lab::record` log line, tests)
- Modify: `bins/scrapix-api/src/lab_client.rs:21,165-177,253-256,317-322,404-441,584-588` (local usage accounting), tests `refreshed_snapshot_still_counts_undelivered_usage`, `BrokenOutbox`, `unreadable_outbox_falls_back_to_the_labs_balance`, `balance_subtracts_local_usage_and_resets_on_refresh`
- Modify: `bins/scrapix-api/src/lib.rs:1283-1316` (`bill_job`), `3405-3452` (`record_usage`, `note_recorded_usage`), `3462-3526` (scrape/map/search usage), `3962,3994-4002` (perform_scrape call), `6760` (`with_undelivered_usage`), `9169-9174` (TerminalStore wrapper), tests at `8345-8460`, `9395-9470`, `9508`, `10277`, `10326-10430`, `10579`
- Modify: `bins/scrapix-api/src/documents.rs:190-230,440-455,1060-1130`
- Modify: `bins/scrapix-api/src/extract.rs:600-620,655-660,840-848,1300-1318`
- Modify: `bins/scrapix-api/src/scrape_tests.rs:88-96,133-190`
- Modify: `bins/scrapix-api/src/lab_sink.rs:176-186` (`ev()` helper only)

**Interfaces:**
- Consumes: nothing new.
- Produces: `LabEvent::usage(account_id: &str, api_key_id: Option<&str>, operation: &str, units: serde_json::Value, description: String, job_id: Option<&str>) -> LabEvent`; `LabEvent::crawl_final_usage(job_id: &str, account_id: &str, units: serde_json::Value, description: String) -> LabEvent`; `LabOutbox` without `undelivered_usage_credits`; `AppState::record_usage(&self, ctx: &AccountContext, operation: &str, units: serde_json::Value, description: String, job_id: Option<&str>)`; `LabClient` without `with_undelivered_usage` / `note_usage`. `billing::scrape_credits`, `MAP_CREDITS`, `SEARCH_CREDITS`, `extract_ai_call_credits`, `ocr_credits` are still called by the pre-checks until Task 4 deletes them.

- [ ] **Step 1: Write the vendored v2 schema**

Create `contracts/vendor/lab/lab-events.schema.json` with exactly this content (derived from spec §4; the Lab will publish the same file at `meilisearch/lab:contracts/lab-events.schema.json`, after which `contracts/check-drift.sh --fix` replaces it byte for byte):

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://lab.meilisearch.com/contracts/lab-events.schema.json",
  "title": "Lab event",
  "description": "One event an engine (Scrapix, Lumen, glutony) reports to the Lab at POST /internal/events. Delivered in batches as {\"events\": [<event>, ...]}, at most 500 per batch. The authenticated X-Lab-Instance-Id header, not the body, names the reporting instance. Owned by meilisearch/lab; engines vendor a byte copy.",
  "type": "object",
  "required": ["id", "type", "occurred_at", "account_id", "api_key_id", "product", "data"],
  "additionalProperties": false,
  "properties": {
    "id": { "$ref": "#/$defs/uuid", "description": "Idempotency key. UUIDv7 per request; UUIDv5 of a stable name for job-final events." },
    "type": { "enum": ["usage.recorded", "job.completed", "job.failed"] },
    "occurred_at": { "type": "string", "format": "date-time" },
    "account_id": { "$ref": "#/$defs/uuid", "description": "The Lab account the usage belongs to." },
    "api_key_id": { "type": ["string", "null"], "description": "The Lab API key id when known." },
    "product": { "enum": ["scrapix", "lumen", "glutony"] },
    "data": { "type": "object" }
  },
  "allOf": [
    {
      "if": { "properties": { "type": { "const": "usage.recorded" } }, "required": ["type"] },
      "then": { "properties": { "data": { "$ref": "#/$defs/usageData" } } }
    },
    {
      "if": { "properties": { "type": { "const": "job.completed" } }, "required": ["type"] },
      "then": { "properties": { "data": { "$ref": "#/$defs/jobCompletedData" } } }
    },
    {
      "if": { "properties": { "type": { "const": "job.failed" } }, "required": ["type"] },
      "then": { "properties": { "data": { "$ref": "#/$defs/jobFailedData" } } }
    },
    {
      "if": { "properties": { "type": { "const": "usage.recorded" }, "product": { "const": "scrapix" } }, "required": ["type", "product"] },
      "then": { "properties": { "data": { "properties": { "operation": { "enum": ["scrape", "map", "search", "parse", "ocr", "extract", "crawl"] } } } } }
    },
    {
      "if": { "properties": { "type": { "const": "usage.recorded" }, "product": { "const": "glutony" } }, "required": ["type", "product"] },
      "then": { "properties": { "data": { "properties": { "operation": { "enum": ["ingest"] } } } } }
    },
    {
      "if": { "properties": { "type": { "const": "usage.recorded" }, "product": { "const": "lumen" } }, "required": ["type", "product"] },
      "then": { "properties": { "data": { "properties": { "operation": { "enum": ["chat", "embed", "rerank", "systemone", "gateway"] } } } } }
    }
  ],
  "$defs": {
    "uuid": {
      "type": "string",
      "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
    },
    "usageData": {
      "type": "object",
      "required": ["operation", "units"],
      "additionalProperties": false,
      "properties": {
        "operation": { "type": "string", "description": "Product-specific operation name (spec 4.3)." },
        "units": {
          "type": "object",
          "description": "Raw units. The Lab prices the names in its pricing.yml and stores the rest.",
          "additionalProperties": { "type": "integer", "minimum": 0 }
        },
        "provider_cost_micro_usd": { "type": "integer", "minimum": 0, "default": 0, "description": "Money the engine paid an upstream provider (LLM tokens, OCR API), in micro-USD; priced by markup." },
        "description": { "type": "string" },
        "job_id": { "type": "string" },
        "credits": { "type": "integer", "minimum": 0, "deprecated": true, "description": "Legacy hosted Scrapix only; used when units price to nothing. Removed next release." }
      }
    },
    "jobCompletedData": {
      "type": "object",
      "required": ["job_id", "index_uid", "pages_crawled", "documents_indexed", "duration_secs"],
      "additionalProperties": false,
      "properties": {
        "job_id": { "type": "string" },
        "index_uid": { "type": "string" },
        "pages_crawled": { "type": "integer", "minimum": 0 },
        "documents_indexed": { "type": "integer", "minimum": 0 },
        "duration_secs": { "type": "integer", "minimum": 0 }
      }
    },
    "jobFailedData": {
      "type": "object",
      "required": ["job_id", "error_message", "pages_crawled"],
      "additionalProperties": false,
      "properties": {
        "job_id": { "type": "string" },
        "error_message": { "type": "string" },
        "pages_crawled": { "type": "integer", "minimum": 0 }
      }
    }
  }
}
```

Then delete the engine-owned copy:

```bash
git rm contracts/lab-events.schema.json
```

- [ ] **Step 2: Point the drift check at both vendored files and tolerate an owner file that does not exist yet**

In `contracts/check-drift.sh` replace line 9 `FILES=(lab-internal.openapi.json)` with:

```bash
FILES=(lab-internal.openapi.json lab-events.schema.json)
```

and replace the body of the `for f in "${FILES[@]}"; do` loop's fetch (the `if [ -n "${LAB_SRC:-}" ]; then ... fi` block) with a version that notices a missing owner file:

```bash
  if [ -n "${LAB_SRC:-}" ]; then
    if [ ! -f "$LAB_SRC/contracts/$f" ]; then
      echo "notice: $LAB_SRC/contracts/$f does not exist yet on the Lab; skipping $vendored"
      continue
    fi
    cp "$LAB_SRC/contracts/$f" "$owner"
  else
    if ! gh api "repos/meilisearch/lab/contents/contracts/$f" -H 'Accept: application/vnd.github.raw' > "$owner" 2>/dev/null; then
      echo "notice: meilisearch/lab main has no contracts/$f yet; skipping $vendored"
      continue
    fi
  fi
```

Run `bash -n contracts/check-drift.sh` (syntax) and `LAB_SRC=/Users/quentindequelen/Projects/Meilisearch/_side_projects/meilisearch-lab/.claude/worktrees/advisor-838e2a contracts/check-drift.sh`. Expected: `ok: vendor/lab/lab-internal.openapi.json` and the notice for `lab-events.schema.json`, exit 0.

- [ ] **Step 3: Update `contracts/README.md`**

Replace the `lab-events.schema.json` bullet under "Engine-owned files" with nothing (delete it), change the intro sentence "which vendors byte copies of `openapi.json` and `lab-events.schema.json`" to "which vendors a byte copy of `openapi.json`", and add under "Vendored from the Lab":

```markdown
- `vendor/lab/lab-events.schema.json`: the events contract (spec: Lab
  platform contract v2 §4) every engine reports against at
  `POST {LAB_URL}/internal/events`: `usage.recorded` carries raw `units` and
  `provider_cost_micro_usd`, never credits; the Lab prices them. Owned by
  `meilisearch/lab` (`contracts/lab-events.schema.json`). Until the Lab
  publishes it, the vendored copy is the text derived from the spec and the
  drift check prints a notice instead of comparing.
```

Also change "that the hosted engine calls with `LAB_SERVICE_TOKEN`" to "that the engine calls with its `LAB_INSTANCE_ID` / `LAB_INSTANCE_SECRET`".

- [ ] **Step 4: Write the failing contract tests in `lab_events.rs`**

In `bins/scrapix-api/src/lab_events.rs` tests module, change `contract_validator()` to read the vendored file and rewrite the three schema tests:

```rust
    /// The engine's serialized events must satisfy the Lab-owned contract
    /// (`contracts/vendor/lab/lab-events.schema.json`), which the Lab's
    /// receiver is also tested against.
    fn contract_validator() -> jsonschema::Validator {
        let raw = include_str!("../../../contracts/vendor/lab/lab-events.schema.json");
        let schema: Value = serde_json::from_str(raw).unwrap();
        jsonschema::validator_for(&schema).unwrap()
    }

    #[test]
    fn events_satisfy_the_contract_schema() {
        let v = contract_validator();
        let acct = "7f1c2a8e-0000-4000-8000-000000000001";
        assert_valid(
            &v,
            &LabEvent::usage(
                acct,
                Some("key_1"),
                "scrape",
                json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 0, "ai_extraction": 0}),
                "https://e.com".into(),
                None,
            ),
        );
        assert_valid(
            &v,
            &LabEvent::usage(acct, None, "extract", json!({"documents": 1}), "extract".into(), Some("job-1")),
        );
        assert_valid(
            &v,
            &LabEvent::crawl_final_usage(
                "job-1",
                acct,
                json!({"pages_http":10,"pages_browser":2,"pages_ai":0,"pages_ocr":0}),
                "Job job-1 (10 http + 2 browser pages, 0 AI-enriched)".into(),
            ),
        );
        assert_valid(
            &v,
            &LabEvent::job_completed(
                "job-1",
                acct,
                json!({"job_id":"job-1","index_uid":"docs","pages_crawled":12,
                       "documents_indexed":12,"duration_secs":30}),
            ),
        );
        assert_valid(
            &v,
            &LabEvent::job_failed(
                "job-1",
                acct,
                json!({"job_id":"job-1","error_message":"boom","pages_crawled":0}),
            ),
        );
    }

    #[test]
    fn usage_events_carry_units_and_provider_cost_and_never_credits() {
        let e = LabEvent::usage(
            "7f1c2a8e-0000-4000-8000-000000000001",
            None,
            "map",
            json!({"requests": 1, "urls_found": 17}),
            "https://e.com".into(),
            None,
        );
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["data"]["units"], json!({"requests": 1, "urls_found": 17}));
        assert_eq!(v["data"]["provider_cost_micro_usd"], 0);
        assert!(v["data"].get("credits").is_none(), "credits are priced by the Lab");
    }

    #[test]
    fn the_contract_schema_rejects_malformed_events() {
        let v = contract_validator();
        let acct = "7f1c2a8e-0000-4000-8000-000000000001";
        let good = serde_json::to_value(LabEvent::usage(
            acct,
            None,
            "map",
            json!({"requests": 1}),
            "m".into(),
            None,
        ))
        .unwrap();
        assert!(v.is_valid(&good));
        let mut bad = good.clone();
        bad["data"]["units"]["requests"] = json!(-1);
        assert!(!v.is_valid(&bad), "negative unit");
        let mut bad = good.clone();
        bad["data"]["units"]["formats"] = json!(["markdown"]);
        assert!(!v.is_valid(&bad), "units are integers only");
        let mut bad = good.clone();
        bad["data"]["provider_cost_micro_usd"] = json!(-5);
        assert!(!v.is_valid(&bad), "negative provider cost");
        let mut bad = good.clone();
        bad["data"]["operation"] = json!("teleport");
        assert!(!v.is_valid(&bad), "unknown scrapix operation");
        let mut bad = good.clone();
        bad["account_id"] = json!("acc");
        assert!(!v.is_valid(&bad), "non-uuid account");
        let mut bad = good;
        bad["type"] = json!("job.completed");
        assert!(!v.is_valid(&bad), "usage data under a job type");
    }
```

Delete the test `usage_credits_only_for_usage_events` and the `undelivered_usage` / `memory_outbox_undelivered_usage` / `sqlite_outbox_undelivered_usage` / `pg_outbox_undelivered_usage` tests. In every remaining test in this file, remove the credits argument from `LabEvent::usage(...)` (the 4th positional argument, an integer) and from `LabEvent::crawl_final_usage(...)` (the 3rd positional argument), e.g. `LabEvent::usage("a", None, "map", 2, json!({}), "m".into(), None)` becomes `LabEvent::usage("a", None, "map", json!({}), "m".into(), None)` and `LabEvent::crawl_final_usage("j", acct, 5, json!({"pages_http":5}), "Job j".into())` becomes `LabEvent::crawl_final_usage("j", acct, json!({"pages_http":5}), "Job j".into())`. In `usage_serializes_to_the_contract_shape` replace `assert_eq!(v["data"]["credits"], 3);` with `assert_eq!(v["data"]["provider_cost_micro_usd"], 0);` and the description `"https://e.com (3 credits)"` with `"https://e.com"`. In `usage_description_is_stripped_of_control_characters` keep the strings but drop the credits arguments.

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test -p scrapix-api lab_events 2>&1 | head -40`
Expected: compile errors (`LabEvent::usage` takes 7 arguments, the vendored include path does not resolve until Step 1 is committed; the include resolves now since the file exists, but the arity mismatch fails).

- [ ] **Step 6: Implement the units-only `LabEvent`**

In `bins/scrapix-api/src/lab_events.rs` replace `usage_data` and the `impl LabEvent` constructors (lines 53-130):

```rust
/// Provider pass-through cost the engine paid for this usage, in micro-USD
/// (spec §4.2). Scrapix does not price its AI/OCR providers yet, so it
/// reports 0; the Lab prices units from its own table.
const PROVIDER_COST_MICRO_USD: i64 = 0;

fn usage_data(operation: &str, units: Value, description: String, job_id: Option<&str>) -> Value {
    let mut d = json!({
        "operation": operation,
        "units": units,
        "provider_cost_micro_usd": PROVIDER_COST_MICRO_USD,
        "description": strip_control_chars(&description),
    });
    if let Some(j) = job_id {
        d["job_id"] = json!(j);
    }
    d
}

impl LabEvent {
    pub fn usage(
        account_id: &str,
        api_key_id: Option<&str>,
        operation: &str,
        units: Value,
        description: String,
        job_id: Option<&str>,
    ) -> Self {
        event(
            Uuid::now_v7(),
            "usage.recorded",
            account_id,
            api_key_id,
            usage_data(operation, units, description, job_id),
        )
    }

    pub fn crawl_final_usage(
        job_id: &str,
        account_id: &str,
        units: Value,
        description: String,
    ) -> Self {
        let id = Uuid::new_v5(&LAB_NAMESPACE, format!("job:{job_id}:final").as_bytes());
        event(
            id,
            "usage.recorded",
            account_id,
            None,
            usage_data("crawl", units, description, Some(job_id)),
        )
    }

    fn lifecycle(kind: &str, job_id: &str, account_id: &str, data: Value) -> Self {
        let id = Uuid::new_v5(&LAB_NAMESPACE, format!("job:{job_id}:lifecycle").as_bytes());
        event(id, kind, account_id, None, data)
    }

    pub fn job_completed(job_id: &str, account_id: &str, data: Value) -> Self {
        Self::lifecycle("job.completed", job_id, account_id, data)
    }

    pub fn job_failed(job_id: &str, account_id: &str, data: Value) -> Self {
        Self::lifecycle("job.failed", job_id, account_id, data)
    }
}
```

(The `#[allow(dead_code)] // wired in Task 5` attributes go away here; the constructors are all called from `lib.rs`.) Delete `usage_credits`. In the `LabOutbox` trait delete the `undelivered_usage_credits` method and its doc comment, and delete the three implementations (`PgOutbox` lines 253-266, `SqliteOutbox` 395-408, `MemoryOutbox` 537-545). Remove `#[allow(dead_code)] // used by lab_sink / charge sites` above the trait. In `Lab::record` replace the per-event log with:

```rust
            for ev in events {
                let operation = ev.data.get("operation").and_then(|v| v.as_str());
                let units = ev.data.get("units").map(|u| u.to_string());
                tracing::warn!(
                    event_id = %ev.id,
                    account_id = %ev.account_id,
                    event_type = %ev.kind,
                    operation,
                    units,
                    "Lab event not recorded"
                );
            }
```

Delete `Lab::outbox()` (`#[allow(dead_code)] // not used yet`, lines 421-424).

- [ ] **Step 7: Remove the local usage accounting from `LabClient`**

In `bins/scrapix-api/src/lab_client.rs`:
- Delete `use crate::lab_events::LabOutbox;` (line 21).
- `Balance` (lines 165-177) loses `used_since` and `available()`; replace with:

```rust
struct Balance {
    balance: i64,
    fetched: Instant,
    ttl: Duration,
    retry_at: Option<Instant>,
}
```

- Delete the `undelivered` field (253-256) and its initializer `undelivered: None,` (312), `with_undelivered_usage` (317-322), `undelivered_usage` (404-422), `note_usage` (584-588).
- `remember_balance` becomes synchronous:

```rust
    /// Take a new balance snapshot. Usage this engine recorded since is in
    /// the outbox and is debited by the Lab when delivered; the engine does
    /// not price it (decision B), so the snapshot is the Lab's number.
    fn remember_balance(&self, account_id: &str, credits: &Option<Credits>, ttl: Duration) {
        if let Some(c) = credits {
            self.balances.lock().unwrap().insert(
                account_id.to_string(),
                Balance {
                    balance: c.balance,
                    fetched: Instant::now(),
                    ttl,
                    retry_at: None,
                },
            );
        }
    }
```

and `to_identity` calls `self.remember_balance(&account_id, &a.credits, ttl);` without `.await`. Every `b.available()` becomes `b.balance` (lines 599, 625, 643, 653). Update the doc comment on `available_credits` to "Spendable credits as the Lab last reported them (from a fresh snapshot, or a new one)".
- Tests: delete `refreshed_snapshot_still_counts_undelivered_usage`, `BrokenOutbox`, `unreadable_outbox_falls_back_to_the_labs_balance`. Rename `balance_subtracts_local_usage_and_resets_on_refresh` to `balance_is_the_labs_number_and_refreshes_after_ttl` with body:

```rust
    #[tokio::test]
    async fn balance_is_the_labs_number_and_refreshes_after_ttl() {
        let (lab, c) = setup().await;
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 100}}),
        );
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(100));
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 70}}),
        );
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(100), "cached");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(70), "fresh snapshot");
    }
```

- [ ] **Step 8: Update the emission sites**

`bins/scrapix-api/src/lib.rs`:

(a) `bill_job` (1283-1316): delete the `features` block, the `credits` computation and the `credits_billed` increment; delete the `credits_billed` field (313) and its initializer (461). The body after `pages_billed` becomes:

```rust
        let (Some(_), Some(acct_id)) = (self.lab.as_ref(), account_id) else {
            return;
        };
        let description = if pages_ocr > 0 {
            format!(
                "Job {} ({} http + {} browser pages, {} AI-enriched, {} OCR pages)",
                job_id, pages_http, pages_browser, pages_ai, pages_ocr
            )
        } else {
            format!(
                "Job {} ({} http + {} browser pages, {} AI-enriched)",
                job_id, pages_http, pages_browser, pages_ai
            )
        };
        let units = serde_json::json!({
            "pages_http": pages_http,
            "pages_browser": pages_browser,
            "pages_ai": pages_ai,
            "pages_ocr": pages_ocr,
        });
        let event = lab_events::LabEvent::crawl_final_usage(job_id, acct_id, units, description);
        self.owe_lab_event(job_id, event);
```

Remove the now-unused `FeaturesConfig` import only if `cargo check` reports it unused (it is used elsewhere in `lib.rs`).

(b) `record_usage` (3405-3431) drops the `credits: i64` parameter and passes `(&ctx.account_id, ctx.api_key_id.as_deref(), operation, units, description, job_id)`. `record_events` (3434-3440) becomes:

```rust
    pub(crate) async fn record_events(&self, events: &[lab_events::LabEvent]) {
        let Some(ref lab) = self.lab else { return };
        let _ = lab.record(events).await;
    }
```

Delete `note_recorded_usage` (3444-3452) and its two other callers at 643 and 774 (`record_unowned_lab_events`, `record_owed_lab_events`): remove the `note_recorded_usage(...)` line in each, and the `let lab_api = self.lab_api.clone();` that only fed it (line 639) if nothing else uses it.

(c) Request usage (3462-3526):

```rust
/// Usage event for one successful scrape: one page, served by the browser
/// or over HTTP, plus the AI work that produced a result.
async fn record_scrape_usage(
    state: &AppState,
    ctx: &AccountContext,
    js_rendered: bool,
    ai_summary: bool,
    ai_extraction: bool,
    final_url: &str,
) {
    state
        .record_usage(
            ctx,
            "scrape",
            serde_json::json!({
                "pages_http": u8::from(!js_rendered),
                "pages_browser": u8::from(js_rendered),
                "ai_summary": u8::from(ai_summary),
                "ai_extraction": u8::from(ai_extraction),
            }),
            final_url.to_string(),
            None,
        )
        .await;
}

/// Usage event for one successful map.
async fn record_map_usage(state: &AppState, ctx: &AccountContext, url: &str, urls_found: usize) {
    state
        .record_usage(
            ctx,
            "map",
            serde_json::json!({ "requests": 1, "urls_found": urls_found }),
            url.to_string(),
            None,
        )
        .await;
}

/// Usage event for one search; `result` is the Meilisearch response.
async fn record_search_usage(
    state: &AppState,
    ctx: &AccountContext,
    url: &str,
    q: &str,
    result: &serde_json::Value,
) {
    let results = result
        .get("hits")
        .and_then(|h| h.as_array())
        .map_or(0, |a| a.len());
    state
        .record_usage(
            ctx,
            "search",
            serde_json::json!({ "requests": 1, "results": results }),
            format!("{url} q={q}"),
            None,
        )
        .await;
}
```

(d) `perform_scrape`: delete line 3962 (`let charged = billing::scrape_credits(...)`) and change the call at 3994-4002 to `record_scrape_usage(state, ctx, js_rendered, has_ai_summary, has_ai_extraction, &final_url).await;` (`js_rendered` is the local from line 3730).

(e) `wire_mode` line 6760: `let lab_api = Arc::new(lab_api.with_undelivered_usage(store.lab_outbox()));` becomes `let lab_api = Arc::new(lab_api);` and delete the comment above it.

(f) Test wrapper at 9169-9174 (`TerminalStore`'s `LabOutbox` impl): delete the `undelivered_usage_credits` method.

`bins/scrapix-api/src/documents.rs`:
- `record_document_usage` (198-230): drop `base_cost: i64`; the document event's units become `serde_json::json!({ "documents": 1 })` and description `label.to_string()`; the OCR event's units `serde_json::json!({ "documents": ocr_billable, "pages_ocr": ocr_billable })` (the Lab prices `ocr` by `documents`, one per recognized page, which keeps today's per-page price) and description `format!("{label} ({ocr_billable} OCR pages)")`. Delete `DocumentJob::credits` (190-196) only after Task 4 removes its last caller (the pre-check at 280-283); here keep it and drop its use at 440-455: the call becomes `record_document_usage(state, ctx, job.operation, &job.label, ocr_billable).await;`.

`bins/scrapix-api/src/extract.rs`:
- `charge` (605-620) becomes `async fn charge(&self, units: serde_json::Value, operation: &str, description: &str)` passing `units` to `record_usage`. Call sites: 659 `self.charge(serde_json::json!({"requests": 1}), "map", input).await;`, 843-847 `self.charge(serde_json::json!({"documents": 1}), "extract", &format!("Extract {} ({what})", self.job_id)).await;`.

- [ ] **Step 9: Update the tests at every emission site**

- `lib.rs` 8345-8460: `record_usage(&ctx(), "scrape", serde_json::json!({"pages_http": 1}), "https://e.com".into(), None)`; assert `events[0].data["units"]["pages_http"] == 1` instead of `credits`; `record_usage_is_a_noop_without_a_lab` drops the `2`; `scrape_usage_event_carries_formats_and_ai_flags` becomes `scrape_usage_event_carries_page_kind_and_ai_flags` calling `record_scrape_usage(&state, &ctx(), true, true, false, "https://e.com/x")` and asserting:

```rust
        assert_eq!(
            events[0].data,
            serde_json::json!({
                "operation": "scrape",
                "units": {"pages_http": 0, "pages_browser": 1, "ai_summary": 1, "ai_extraction": 0},
                "provider_cost_micro_usd": 0,
                "description": "https://e.com/x",
            })
        );
```

  map: `{"operation": "map", "units": {"requests": 1, "urls_found": 17}, "provider_cost_micro_usd": 0, "description": "https://e.com"}`; search: `{"operation": "search", "units": {"requests": 1, "results": 3}, "provider_cost_micro_usd": 0, "description": "https://e.com q=rust"}`.
- `lib.rs` 9395-9470 `every_record_site_feeds_the_balance_snapshot`: delete the test (the balance no longer subtracts local usage; Task 4 pins the pre-check against the Lab's number).
- `lib.rs` 9508 and 10414: delete the `credits_billed` assertions; at 10326 rename the test to `completed_job_with_mixed_delivery_reports_delivered_units` and replace the `usage[0].data["credits"] == 15` assertion with `assert_eq!(usage[0].data["units"], serde_json::json!({"pages_http": 1, "pages_browser": 2, "pages_ai": 1, "pages_ocr": 0}));`; update its doc comment to say units, not credits.
- `lib.rs` 10277: `assert_eq!(usage[0].data["units"]["pages_http"], 1);` already follows; delete the `credits` line. 10579: replace `assert_eq!(usage[0].data["credits"], 3);` with `assert_eq!(usage[0].data["units"]["pages_http"], 3);`.
- `documents.rs` 1060-1130: `record_document_usage(&state, &usage_ctx(), "parse", "upload://a.pdf", 0)`; expected data `{"operation": "parse", "units": {"documents": 1}, "provider_cost_micro_usd": 0, "description": "upload://a.pdf"}`; the OCR test expects `events[1].data == {"operation": "ocr", "units": {"documents": 2, "pages_ocr": 2}, "provider_cost_micro_usd": 0, "description": "https://e.com/s.pdf (2 OCR pages)"}`; `document_response_records_usage_for_the_caller` asserts `events[0].data["units"]["documents"] == 1`.
- `extract.rs` 1300-1318: `runner.charge(serde_json::json!({"documents": 3}), "extract", "extract: 3 pages").await;` and expected data `{"operation": "extract", "units": {"documents": 3}, "provider_cost_micro_usd": 0, "description": "extract: 3 pages", "job_id": "job-1"}`.
- `scrape_tests.rs` 88-96: `charged()` returns `Vec<serde_json::Value>` of `e.data["units"].clone()`; the three assertions become `vec![serde_json::json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 0, "ai_extraction": 0})]` (no AI), the same for the failed AI call, and `{"pages_http": 1, "pages_browser": 0, "ai_summary": 1, "ai_extraction": 0}` for the successful summary.
- `lab_sink.rs` `ev()` (176-186): drop the `2`.

- [ ] **Step 10: Run the crate's tests**

Run: `cargo test -p scrapix-api 2>&1 | tail -20`
Expected: all pass (Postgres tests print `skipped` without a live database).

- [ ] **Step 11: Pre-commit and commit**

```bash
cargo fmt && cargo check && cargo clippy
git add -A contracts bins/scrapix-api
git commit -m "feat(lab): vendor the Lab-owned events schema v2 and report units, not credits"
```

---

### Task 2: `LAB_INSTANCE_ID` / `LAB_INSTANCE_SECRET` settings

**Files:**
- Modify: `bins/scrapix-api/src/lib.rs:161-175` (`Args`), `bins/scrapix/src/all.rs:392-425` (`build_api_args`)
- Modify: `bins/scrapix-api/src/settings.rs` (`LabSettings`, `resolve`, tests)

**Interfaces:**
- Produces: `Args.lab_instance_id: Option<String>`, `Args.lab_instance_secret: Option<String>`; `LabSettings { url: String, instance_id: String, instance_secret: String, service_token: String }`; `EngineSettings.lab` is `Some` iff hosted (unchanged).

- [ ] **Step 1: Write the failing settings tests**

In `bins/scrapix-api/src/settings.rs` tests: add the two fields to `args()` (`parsed.lab_instance_id = get("LAB_INSTANCE_ID"); parsed.lab_instance_secret = get("LAB_INSTANCE_SECRET");`), add constants and replace `LAB`:

```rust
    const INSTANCE_ID: &str = "0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f";
    const INSTANCE_SECRET: &str =
        "abababababababababababababababababababababababababababababababab";
    const LAB: [(&str, &str); 4] = [
        ("LAB_URL", "http://127.0.0.1:8091"),
        ("LAB_INSTANCE_ID", INSTANCE_ID),
        ("LAB_INSTANCE_SECRET", INSTANCE_SECRET),
        ("LAB_SERVICE_TOKEN", SECRET32),
    ];
```

Replace `hosted_requires_lab_url_secret_and_token` and `hosted_falls_back_to_lab_events_url_and_derives_the_base` (keep the latter but with the new `LAB` entries instead of `LAB_EVENTS_SECRET`) and add:

```rust
    #[test]
    fn hosted_requires_lab_url_instance_credentials_and_service_token() {
        let e = EngineSettings::resolve(&args(&[("SCRAPIX_MODE", "hosted")])).unwrap_err();
        assert!(e.0.contains("LAB_URL"), "{}", e.0);
        let mut no_id = hosted(&[]);
        no_id.retain(|(k, _)| *k != "LAB_INSTANCE_ID");
        assert!(EngineSettings::resolve(&args(&no_id)).unwrap_err().0.contains("LAB_INSTANCE_ID"));
        let mut bad_id = hosted(&[]);
        bad_id[2] = ("LAB_INSTANCE_ID", "not-a-uuid");
        assert!(EngineSettings::resolve(&args(&bad_id)).unwrap_err().0.contains("uuid"));
        let mut short = hosted(&[]);
        short[3] = ("LAB_INSTANCE_SECRET", "abcdef");
        assert!(EngineSettings::resolve(&args(&short)).unwrap_err().0.contains("64 hex"));
        let mut not_hex = hosted(&[]);
        not_hex[3] = ("LAB_INSTANCE_SECRET", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz");
        assert!(EngineSettings::resolve(&args(&not_hex)).unwrap_err().0.contains("64 hex"));
        let mut no_token = hosted(&[]);
        no_token.retain(|(k, _)| *k != "LAB_SERVICE_TOKEN");
        assert!(EngineSettings::resolve(&args(&no_token)).unwrap_err().0.contains("LAB_SERVICE_TOKEN"));
        let mut bad = hosted(&[]);
        bad[1] = ("LAB_URL", "127.0.0.1:8091");
        assert!(EngineSettings::resolve(&args(&bad)).unwrap_err().0.contains("LAB_URL"));
    }

    #[test]
    fn hosted_keeps_the_instance_credentials_and_the_service_token() {
        let s = EngineSettings::resolve(&args(&hosted(&[]))).unwrap();
        let lab = s.lab.unwrap();
        assert_eq!(lab.instance_id, INSTANCE_ID);
        assert_eq!(lab.instance_secret, INSTANCE_SECRET);
        assert_eq!(lab.service_token.as_deref(), Some(SECRET32));
    }

    #[test]
    fn hosted_accepts_but_ignores_lab_events_secret() {
        let s = EngineSettings::resolve(&args(&hosted(&[("LAB_EVENTS_SECRET", SECRET32)]))).unwrap();
        assert!(s.lab.is_some());
    }

    #[test]
    fn standalone_refuses_instance_credentials() {
        // Spec 3.3: no "lab-connected standalone". A hosted env pasted onto a
        // standalone engine must not run unbilled.
        let e = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("LAB_URL", "http://127.0.0.1:8091"),
            ("LAB_INSTANCE_ID", INSTANCE_ID),
            ("LAB_INSTANCE_SECRET", INSTANCE_SECRET),
        ]))
        .unwrap_err();
        assert!(e.0.contains("LAB_INSTANCE_ID"), "{}", e.0);
        assert!(e.0.contains("SCRAPIX_MODE=hosted"), "{}", e.0);
        let e = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("LAB_INSTANCE_SECRET", INSTANCE_SECRET),
        ]))
        .unwrap_err();
        assert!(e.0.contains("LAB_INSTANCE_SECRET"), "{}", e.0);
    }
```

Keep `standalone_ignores_lab_url` / `standalone_ignores_lab_settings` as they are (a lone `LAB_URL` is still ignored).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p scrapix-api settings 2>&1 | head -20`
Expected: compile errors on `lab_instance_id` / `instance_id`.

- [ ] **Step 3: Add the `Args` fields**

In `bins/scrapix-api/src/lib.rs` after `lab_service_token` (line 175) add:

```rust
    /// The instance id the Lab minted for this hosted engine deployment
    /// (uuid). Required in hosted mode, refused in standalone mode.
    #[arg(long, env = "LAB_INSTANCE_ID")]
    pub lab_instance_id: Option<String>,

    /// The secret the Lab minted with LAB_INSTANCE_ID (64 hex chars).
    #[arg(long, env = "LAB_INSTANCE_SECRET", hide_env_values = true)]
    pub lab_instance_secret: Option<String>,
```

Change the `lab_events_secret` doc to `/// Deprecated and ignored: event batches are signed with LAB_INSTANCE_SECRET.` and `lab_service_token` doc to `/// Hosted only (min 32 chars): the token the Lab presents when it calls this engine for an account (X-Scrapix-Account-Id).`

In `bins/scrapix/src/all.rs` `build_api_args` add after `lab_service_token`:

```rust
        lab_instance_id: std::env::var("LAB_INSTANCE_ID").ok(),
        lab_instance_secret: std::env::var("LAB_INSTANCE_SECRET").ok(),
```

- [ ] **Step 4: Implement the settings**

In `bins/scrapix-api/src/settings.rs` replace `LabSettings` (33-43):

```rust
/// How the hosted engine reaches the Lab (`Some` iff hosted). A standalone
/// engine has no Lab.
#[derive(Debug, Clone)]
pub struct LabSettings {
    /// The Lab base URL (no trailing `/`): events go to
    /// `{url}/internal/events`, lookups to `{url}/internal/*`.
    pub url: String,
    /// `LAB_INSTANCE_ID`: sent as `X-Lab-Instance-Id` on every call.
    pub instance_id: String,
    /// `LAB_INSTANCE_SECRET`: Bearer on service calls, HMAC key on events.
    pub instance_secret: String,
    /// `LAB_SERVICE_TOKEN`, which the Lab presents when it calls this
    /// engine for an account. Never sent to the Lab.
    pub service_token: String,
}
```

Add helpers after `is_postgres`:

```rust
const INSTANCE_SECRET_LEN: usize = 64;

fn lab_url_from(args: &Args) -> Result<Option<String>, ConfigError> {
    let url = match (non_empty(&args.lab_url), non_empty(&args.lab_events_url)) {
        (Some(u), _) => u,
        (None, Some(e)) => {
            tracing::warn!("LAB_EVENTS_URL is deprecated: set LAB_URL to the Lab base URL");
            crate::lab_client::LabClient::base_from_events_url(&e)
        }
        (None, None) => return Ok(None),
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return err(format!("LAB_URL must start with http:// or https://, got `{url}`"));
    }
    Ok(Some(url.trim_end_matches('/').to_string()))
}

/// Hosted: `LAB_INSTANCE_ID` (uuid) and `LAB_INSTANCE_SECRET` (64 hex
/// chars), both required.
fn instance_credentials(args: &Args) -> Result<(String, String), ConfigError> {
    let id = non_empty(&args.lab_instance_id);
    let secret = non_empty(&args.lab_instance_secret);
    match (id, secret) {
        (None, _) => err("SCRAPIX_MODE=hosted requires LAB_INSTANCE_ID (minted by the Lab: bin/rails lab:hosted_engine:create)"),
        (_, None) => err("SCRAPIX_MODE=hosted requires LAB_INSTANCE_SECRET (minted with LAB_INSTANCE_ID by the Lab)"),
        (Some(id), Some(secret)) => {
            if uuid::Uuid::parse_str(&id).is_err() {
                return err(format!("LAB_INSTANCE_ID must be a uuid, got `{id}`"));
            }
            if secret.len() != INSTANCE_SECRET_LEN || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
                return err("LAB_INSTANCE_SECRET must be the 64 hex characters the Lab minted");
            }
            Ok((id, secret))
        }
    }
}

fn warn_ignored_events_secret(args: &Args) {
    if non_empty(&args.lab_events_secret).is_some() {
        tracing::warn!(
            "LAB_EVENTS_SECRET is ignored: event batches are signed with LAB_INSTANCE_SECRET (unset it)"
        );
    }
}
```

In `resolve`, the `Mode::Hosted` branch (127-183) becomes:

```rust
            Mode::Hosted => {
                if auth_disabled {
                    return err("SCRAPIX_AUTH=disabled is not allowed with SCRAPIX_MODE=hosted");
                }
                let url = lab_url_from(args)?.ok_or_else(|| {
                    ConfigError(
                        "SCRAPIX_MODE=hosted requires LAB_URL (the Lab base URL, e.g. http://127.0.0.1:8091)".into(),
                    )
                })?;
                if non_empty(&args.jwt_secret).is_some() {
                    tracing::info!("JWT_SECRET is ignored: the Lab verifies sessions");
                }
                warn_ignored_events_secret(args);
                let (instance_id, instance_secret) = instance_credentials(args)?;
                let service_token = match non_empty(&args.lab_service_token) {
                    Some(s) if s.chars().count() >= MIN_LAB_SECRET_LEN => s,
                    Some(_) => {
                        return err(format!(
                            "LAB_SERVICE_TOKEN must be at least {MIN_LAB_SECRET_LEN} characters"
                        ))
                    }
                    None => return err("SCRAPIX_MODE=hosted requires LAB_SERVICE_TOKEN (the Lab presents it when it calls this engine)"),
                };
                let lab = LabSettings {
                    url,
                    instance_id,
                    instance_secret,
                    service_token,
                };
                if database_url.is_none() {
                    tracing::warn!(
                        "SCRAPIX_MODE=hosted without DATABASE_URL: the engine uses its default SQLite \
                         file ({DEFAULT_SQLITE_URL}). Undelivered Lab events (usage to bill) and job \
                         history live there: keep it on persistent storage, or set DATABASE_URL"
                    );
                }
                Ok(Self {
                    mode,
                    auth: AuthSetting::Lab,
                    store: store_from(database_url)?,
                    meilisearch,
                    lab: Some(lab),
                })
            }
```

The `Mode::Standalone` branch: before the existing `LAB_* variables are ignored` block (188-194) add the refusal, and extend that block's condition:

```rust
                if non_empty(&args.lab_instance_id).is_some()
                    || non_empty(&args.lab_instance_secret).is_some()
                {
                    return err(
                        "LAB_INSTANCE_ID/LAB_INSTANCE_SECRET are hosted-engine credentials: set SCRAPIX_MODE=hosted, or unset them (a standalone engine has no Lab)",
                    );
                }
                if non_empty(&args.lab_url).is_some()
                    || non_empty(&args.lab_events_url).is_some()
                    || non_empty(&args.lab_events_secret).is_some()
                    || non_empty(&args.lab_service_token).is_some()
                {
                    tracing::info!("LAB_* variables are ignored in standalone mode");
                }
```

The final `Ok(Self { ..., lab: None })` is unchanged. Keep the `MIN_LAB_SECRET_LEN` constant (used by the service token check).

- [ ] **Step 5: Run the settings tests**

Run: `cargo test -p scrapix-api settings`
Expected: PASS. (`lib.rs` still compiles because `wire_mode` reads `lab_cfg.service_token` as a `String` at line 6739; change that call to `LabClient::new(&lab_cfg.url, &lab_cfg.instance_id, &lab_cfg.instance_secret)` only in Task 3: for now make it compile with `lab_api = lab_client::LabClient::new(&lab_cfg.url, &lab_cfg.instance_secret);` and `AuthState::new(lab_api.clone(), Some(lab_cfg.service_token.clone()))`, and the sink's `cfg.events_secret.clone()` at 7198 with `cfg.instance_secret.clone()`.)

- [ ] **Step 6: Pre-commit and commit**

```bash
cargo fmt && cargo check && cargo clippy
git add bins/scrapix-api/src/settings.rs bins/scrapix-api/src/lib.rs bins/scrapix/src/all.rs
git commit -m "feat(settings): LAB_INSTANCE_ID and LAB_INSTANCE_SECRET replace the global pair engine-to-Lab (hosted only)"
```

---

### Task 3: Instance identity on every Lab call, `instances/me` at boot, v2 event delivery

**Files:**
- Modify: `bins/scrapix-api/src/lab_client.rs` (`LabError`, `LabClient` fields/ctor/`request`, new `instances_me`, `testing::FakeLab`, tests)
- Modify: `bins/scrapix-api/src/lab_sink.rs` (headers, batch, 24 h drop, tests)
- Modify: `bins/scrapix-api/src/lab_events.rs` (`LabOutbox::abandon` + 3 impls, tests)
- Modify: `bins/scrapix-api/src/lib.rs:6736-6775` (`wire_mode` hosted branch), `7193-7201` (sink wiring)
- Modify: call sites of `LabClient::new` / `with_timing` in tests: `bins/scrapix-api/src/billing.rs`, `auth/middleware.rs`, `meili.rs`, `lib.rs` (17 sites, `grep -n "LabClient::new(\|LabClient::with_timing("`)

**Interfaces:**
- Consumes: `LabSettings` from Task 2.
- Produces: `LabClient::new(base_url: &str, instance_id: &str, instance_secret: &str)`; `LabClient::with_timing(base_url, instance_id, instance_secret, timing)`; `LabError::CredentialsRejected` (was `ServiceTokenRejected`); `pub(crate) struct InstanceInfo { instance_id: String, kind: String, product: String, region: Option<String>, lab_url: String }` (spec §3.6; `kind` is always `"hosted"`, anything else aborts boot); `LabClient::instances_me(&self) -> Result<InstanceInfo, LabError>`; `testing::INSTANCE_ID`, `testing::SECRET`, `FakeLab::hosted_me()`; `LabSink::new(outbox, client, url, instance_id: String, secret: String)`; `LabOutbox::abandon(&self, ids: &[Uuid]) -> Result<u64, StoreError>`; `lab_sink::BATCH = 500`.

- [ ] **Step 1: Write the failing client tests**

In `lab_client.rs` `testing`: replace `TOKEN` with

```rust
    pub(crate) const INSTANCE_ID: &str = "0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f";
    pub(crate) const SECRET: &str =
        "abababababababababababababababababababababababababababababababab";
```

add `pub me: Mutex<Value>` to `FakeLabState` (initialised in `start()` to `FakeLab::hosted_me()`; a test that needs another answer writes `*lab.state.me.lock().unwrap() = ...`), change `authorized` to:

```rust
    fn authorized(h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok())
            == Some(format!("Bearer {SECRET}").as_str())
            && h.get("x-lab-instance-id").and_then(|v| v.to_str().ok()) == Some(INSTANCE_ID)
    }
```

add the route (before `.with_state`):

```rust
                    .route(
                        "/internal/instances/me",
                        get(
                            |State(s): State<Arc<FakeLabState>>, h: HeaderMap| async move {
                                gate(&s, &h).await?;
                                Ok::<_, StatusCode>(Json(s.me.lock().unwrap().clone()))
                            },
                        ),
                    )
```

and helpers on `FakeLab`:

```rust
        pub(crate) fn hosted_me() -> Value {
            json!({"instance_id": INSTANCE_ID, "kind": "hosted", "product": "scrapix",
                   "region": "eu-west-1", "lab_url": "http://lab"})
        }
```

Since `FakeLabState` derives `Default` and `Mutex<Value>` defaults to `Null`, set `me` in `start()`: `let state = Arc::new(FakeLabState { me: Mutex::new(FakeLab::hosted_me()), ..Default::default() });`.

In the tests module, `setup()` and every `LabClient::with_timing(&lab.url, TOKEN, fast())` become `LabClient::with_timing(&lab.url, INSTANCE_ID, SECRET, fast())` (import `use super::testing::{FakeLab, INSTANCE_ID, SECRET};`). Replace `ping_ok_and_wrong_token_is_rejected` and `ping_404_is_bad_response` with:

```rust
    #[tokio::test]
    async fn instances_me_describes_the_deployment_and_a_wrong_secret_is_rejected() {
        let lab = FakeLab::start().await;
        let me = LabClient::with_timing(&lab.url, INSTANCE_ID, SECRET, fast())
            .instances_me()
            .await
            .unwrap();
        assert_eq!(me.kind, "hosted");
        assert_eq!(me.product, "scrapix");
        assert_eq!(me.region.as_deref(), Some("eu-west-1"));
        let err = LabClient::with_timing(&lab.url, INSTANCE_ID, "wrong", fast())
            .instances_me()
            .await
            .unwrap_err();
        assert_eq!(err, LabError::CredentialsRejected);
        let err = LabClient::with_timing(&lab.url, "22222222-2222-4222-8222-222222222222", SECRET, fast())
            .instances_me()
            .await
            .unwrap_err();
        assert_eq!(err, LabError::CredentialsRejected, "the id is part of the credential");
    }

    #[tokio::test]
    async fn every_call_carries_the_instance_id_header() {
        let (lab, c) = setup().await;
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        assert!(c.introspect(CredentialKind::ApiKey, "sk_live_a", None).await.unwrap().is_some());
        assert!(c.account(ACCT).await.is_ok());
        // `authorized` in the fake requires X-Lab-Instance-Id on every gated
        // route; a client without it would have been 401 (CredentialsRejected).
        assert_eq!(lab.calls(), 2);
    }

    #[tokio::test]
    async fn a_404_on_instances_me_is_bad_response() {
        let (lab, c) = setup().await;
        lab.state
            .status_override
            .store(404, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(c.instances_me().await.unwrap_err(), LabError::BadResponse(404));
    }
```

In `redirects_are_never_followed` replace `c.ping().await` with `c.instances_me().await` (expect `Err(LabError::BadResponse(307))`). Everywhere `LabError::ServiceTokenRejected` appears in tests, use `LabError::CredentialsRejected`.

- [ ] **Step 2: Write the failing sink and outbox tests**

In `lab_events.rs` tests, extend `outbox_roundtrip` before the `purge_delivered` assertion:

```rust
        let gone = LabEvent::usage(acct, None, "map", json!({"requests": 1}), "old".into(), None);
        o.enqueue(std::slice::from_ref(&gone)).await.unwrap();
        assert_eq!(o.abandon(&[gone.id, e.id]).await.unwrap(), 1, "only undelivered rows are dropped");
        assert_eq!(o.pending_stats().await.unwrap(), (0, None));
```

In `lab_sink.rs` tests: `SECRET` becomes the 64-hex string from Task 2's tests, add `const INSTANCE_ID: &str = "0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f";`, `sink()` passes `INSTANCE_ID.into(), SECRET.into()`. Replace `signs_raw_body_and_marks_accepted_delivered` with:

```rust
    #[tokio::test]
    async fn signature_covers_timestamp_dot_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/events"))
            .and(header_exists("X-Lab-Signature"))
            .and(header_exists("X-Lab-Timestamp"))
            .and(wiremock::matchers::header("X-Lab-Instance-Id", INSTANCE_ID))
            .respond_with(AcceptAll)
            .expect(1)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        outbox.enqueue(&[ev(), ev()]).await.unwrap();
        let s = sink(outbox.clone(), format!("{}/internal/events", server.uri()));
        let before = chrono::Utc::now().timestamp();
        assert_eq!(s.deliver_once().await.unwrap(), 2);
        assert!(outbox.due(10).await.unwrap().is_empty());
        let req = &server.received_requests().await.unwrap()[0];
        let ts = req.headers.get("X-Lab-Timestamp").unwrap().to_str().unwrap();
        let ts_num: i64 = ts.parse().unwrap();
        assert!((before..=chrono::Utc::now().timestamp()).contains(&ts_num), "signed with the current time");
        let sig = req.headers.get("X-Lab-Signature").unwrap().to_str().unwrap();
        let mut signed = format!("{ts}.").into_bytes();
        signed.extend_from_slice(&req.body);
        assert_eq!(sig, crate::webhooks::sign_sha256(SECRET.as_bytes(), &signed));
        assert!(req.headers.get("X-Scrapix-Signature").is_none(), "v1 header gone");
    }

    #[tokio::test]
    async fn batches_hold_up_to_500_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(AcceptAll)
            .expect(2)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        let events: Vec<_> = (0..501).map(|_| ev()).collect();
        outbox.enqueue(&events).await.unwrap();
        let s = sink(outbox.clone(), server.uri());
        assert_eq!(s.deliver_once().await.unwrap(), 500);
        assert_eq!(s.deliver_once().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn events_older_than_24h_are_dropped_with_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .expect(0)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        let mut stale = ev();
        stale.occurred_at = chrono::Utc::now() - chrono::TimeDelta::hours(25);
        outbox.enqueue(&[stale.clone()]).await.unwrap();
        let s = sink(outbox.clone(), server.uri());
        let dropped = scrapix_core::metrics::lab_events_delivered_total().with_label_values(&["dropped"]);
        let before = dropped.get();
        assert_eq!(s.deliver_once().await.unwrap(), 0);
        assert_eq!(outbox.pending_stats().await.unwrap().0, 0, "abandoned, not retried");
        assert!(dropped.get() > before);
    }
```

`drains_backlog_in_consecutive_batches` keeps 250 events but now expects `.expect(1)` (one 500-batch) and still asserts order.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p scrapix-api lab_ 2>&1 | head -30`
Expected: compile errors (`INSTANCE_ID`, `instances_me`, `abandon`, `CredentialsRejected` missing).

- [ ] **Step 4: Implement the client**

In `lab_client.rs`:
- Module doc line 3: "authenticated with the instance credentials (`X-Lab-Instance-Id` + Bearer `LAB_INSTANCE_SECRET`)".
- `LabError::ServiceTokenRejected` becomes `CredentialsRejected` with Display `"Lab rejected LAB_INSTANCE_ID/LAB_INSTANCE_SECRET"`; `log_lab_error` message: `"The Lab rejected this engine's LAB_INSTANCE_ID/LAB_INSTANCE_SECRET: re-issue them on the Lab with bin/rails lab:hosted_engine:create"`.
- Fields: replace `token: String` with `instance_id: String, secret: String`; constructors:

```rust
    pub(crate) fn new(base_url: &str, instance_id: &str, instance_secret: &str) -> Self {
        Self::with_timing(base_url, instance_id, instance_secret, Timing::default())
    }

    pub(crate) fn with_timing(
        base_url: &str,
        instance_id: &str,
        instance_secret: &str,
        timing: Timing,
    ) -> Self {
```

with `instance_id: instance_id.to_string(), secret: instance_secret.to_string(),` in the struct literal.
- `request()`: `req.bearer_auth(&self.token)` becomes `req.header("X-Lab-Instance-Id", &self.instance_id).bearer_auth(&self.secret)`; `401 => Err(LabError::CredentialsRejected)`; the `count` match arm names `CredentialsRejected`.
- Delete `ping()` (530-544) and add:

```rust
/// `GET /internal/instances/me` (spec §3.6): what the Lab knows about this
/// deployment. Called once at boot to confirm the credentials and log the
/// identity; `kind` is `"hosted"` (there is no other kind of engine).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct InstanceInfo {
    pub instance_id: String,
    pub kind: String,
    pub product: String,
    #[serde(default)]
    pub region: Option<String>,
    pub lab_url: String,
}
```

and in `impl LabClient`:

```rust
    pub(crate) async fn instances_me(&self) -> Result<InstanceInfo, LabError> {
        let (status, body) = self
            .request("instances_me", self.http.get(format!("{}/internal/instances/me", self.base)))
            .await?;
        if !(200..300).contains(&status) {
            count("instances_me", "unavailable");
            return Err(LabError::Unavailable(format!("HTTP {status}")));
        }
        let me: InstanceInfo = serde_json::from_slice(&body).map_err(|e| {
            count("instances_me", "unavailable");
            LabError::Unavailable(format!("malformed: {e}"))
        })?;
        count("instances_me", "ok");
        Ok(me)
    }
```

- Update the 17 constructor call sites outside this file: `LabClient::new(&lab.url, TOKEN)` becomes `LabClient::new(&lab.url, testing::INSTANCE_ID, testing::SECRET)` (adjust each file's `use crate::lab_client::testing::{...}` import; `middleware.rs` tests also keep their own `SERVICE` constant for the inbound token).

- [ ] **Step 5: Implement `abandon` and the sink**

`lab_events.rs` trait:

```rust
    /// Delete undelivered events the Lab never acknowledged in time (spec
    /// §3.4: permanently rejected after 24 h). Returns how many were dropped.
    async fn abandon(&self, ids: &[Uuid]) -> Result<u64, StoreError>;
```

`PgOutbox`:

```rust
    async fn abandon(&self, ids: &[Uuid]) -> Result<u64, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        sqlx::query("DELETE FROM lab_events WHERE id = ANY($1) AND delivered_at IS NULL")
            .bind(ids)
            .execute(&self.pool)
            .await
            .map(|r| r.rows_affected())
            .map_err(db)
    }
```

`SqliteOutbox`:

```rust
    async fn abandon(&self, ids: &[Uuid]) -> Result<u64, StoreError> {
        let mut dropped = 0;
        let mut tx = self.pool.begin().await.map_err(db)?;
        for id in ids {
            dropped += sqlx::query("DELETE FROM lab_events WHERE id = ? AND delivered_at IS NULL")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db)?
                .rows_affected();
        }
        tx.commit().await.map_err(db)?;
        Ok(dropped)
    }
```

`MemoryOutbox`:

```rust
    async fn abandon(&self, ids: &[Uuid]) -> Result<u64, StoreError> {
        let mut rows = self.rows.lock();
        let before = rows.len();
        rows.retain(|r| r.delivered || !ids.contains(&r.event.id));
        Ok((before - rows.len()) as u64)
    }
```

The `TerminalStore` wrapper in `lib.rs` (around 9155-9175) gets a delegating `abandon`.

`lab_sink.rs`:

```rust
pub const BATCH: i64 = 500;
/// Spec §3.4: an event the Lab never acknowledged for this long is
/// permanently rejected; drop it (error log) instead of retrying forever.
const MAX_AGE: chrono::TimeDelta = chrono::TimeDelta::hours(24);

pub struct LabSink {
    outbox: Arc<dyn LabOutbox>,
    client: reqwest::Client,
    url: String,
    instance_id: String,
    secret: String,
}
```

`new(outbox, client, url, instance_id: String, secret: String)`. `deliver_once`:

```rust
    pub async fn deliver_once(&self) -> Result<usize, StoreError> {
        let now = chrono::Utc::now();
        let (expired, events): (Vec<_>, Vec<_>) = self
            .outbox
            .due(BATCH)
            .await?
            .into_iter()
            .partition(|e| now - e.occurred_at > MAX_AGE);
        if !expired.is_empty() {
            for e in &expired {
                error!(
                    event_id = %e.id,
                    account_id = %e.account_id,
                    event_type = %e.kind,
                    operation = e.data.get("operation").and_then(|v| v.as_str()),
                    occurred_at = %e.occurred_at,
                    "Lab event never acknowledged for 24 h: dropped (spec 3.4)"
                );
            }
            let ids: Vec<Uuid> = expired.iter().map(|e| e.id).collect();
            let dropped = self.outbox.abandon(&ids).await?;
            scrapix_core::metrics::lab_events_delivered_total()
                .with_label_values(&["dropped"])
                .inc_by(dropped as f64);
        }
        if events.is_empty() {
            return Ok(0);
        }
        let ids: Vec<Uuid> = events.iter().map(|e| e.id).collect();
        let body = serde_json::to_vec(&serde_json::json!({ "events": events }))
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let mut signed = format!("{timestamp}.").into_bytes();
        signed.extend_from_slice(&body);
        let signature = crate::webhooks::sign_sha256(self.secret.as_bytes(), &signed);
        let (accepted, outcome): (Vec<Uuid>, &str) = match self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("X-Lab-Instance-Id", &self.instance_id)
            .header("X-Lab-Timestamp", &timestamp)
            .header("X-Lab-Signature", signature)
            .timeout(REQUEST_TIMEOUT)
            .body(body)
            .send()
            .await
        {
```

(the rest of the match and the bookkeeping are unchanged). Add `error` to the `tracing` import. In `spawn`, the `full` check stays `n as i64 == BATCH`.

- [ ] **Step 6: Wire the boot check**

In `lib.rs` `wire_mode` hosted branch replace the `ping` match (6739-6755) with:

```rust
            let lab_api = lab_client::LabClient::new(
                &lab_cfg.url,
                &lab_cfg.instance_id,
                &lab_cfg.instance_secret,
            );
            match lab_api.instances_me().await {
                Ok(me) if me.kind == "hosted" && me.product == "scrapix" => {
                    info!(url = %lab_cfg.url, instance_id = %me.instance_id, region = me.region.as_deref().unwrap_or("-"), "Lab reachable; hosted Scrapix engine")
                }
                Ok(me) => anyhow::bail!(
                    "LAB_INSTANCE_ID {} is a {} {} deployment; this engine needs a hosted scrapix credential (bin/rails lab:hosted_engine:create PRODUCT=scrapix on the Lab)",
                    me.instance_id,
                    me.kind,
                    me.product
                ),
                Err(lab_client::LabError::CredentialsRejected) => anyhow::bail!(
                    "the Lab at {} rejected LAB_INSTANCE_ID/LAB_INSTANCE_SECRET: re-issue them from the Lab",
                    lab_cfg.url
                ),
                Err(lab_client::LabError::BadResponse(404)) => anyhow::bail!(
                    "the Lab at LAB_URL ({}) has no GET /internal/instances/me: either LAB_URL is wrong or the Lab predates contract v2",
                    lab_cfg.url
                ),
                Err(lab_client::LabError::BadResponse(code)) => anyhow::bail!(
                    "the Lab at LAB_URL ({}) answered HTTP {code}: check LAB_URL points at the Lab base URL",
                    lab_cfg.url
                ),
                Err(e) => warn!(
                    error = %e,
                    url = %lab_cfg.url,
                    "Lab unreachable at startup; serving 503s until it answers"
                ),
            }
```

Sink wiring (7193-7201): `lab_sink::LabSink::new(outbox, reqwest::Client::new(), format!("{}/internal/events", cfg.url), cfg.instance_id.clone(), cfg.instance_secret.clone())`.

Add a startup test next to `hosted_wiring_*` tests (search `w.lab_outbox.is_some()` at ~8112 for the existing hosted wiring test and mirror its setup):

```rust
    #[tokio::test]
    async fn a_404_on_instances_me_aborts_startup() {
        let lab = lab_client::testing::FakeLab::start().await;
        lab.state
            .status_override
            .store(404, std::sync::atomic::Ordering::SeqCst);
        let settings = settings::EngineSettings {
            mode: settings::Mode::Hosted,
            auth: settings::AuthSetting::Lab,
            store: settings::StoreUrl::Sqlite("sqlite::memory:".into()),
            meilisearch: None,
            lab: Some(settings::LabSettings {
                url: lab.url.clone(),
                instance_id: lab_client::testing::INSTANCE_ID.into(),
                instance_secret: lab_client::testing::SECRET.into(),
                service_token: "0123456789abcdef0123456789abcdef".into(),
            }),
        };
        let err = wire_mode(&settings).await.unwrap_err().to_string();
        assert!(err.contains("/internal/instances/me"), "{err}");
    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p scrapix-api 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 8: Pre-commit and commit**

```bash
cargo fmt && cargo check && cargo clippy
git add bins/scrapix-api
git commit -m "feat(lab): authenticate every Lab call with the instance credentials and deliver v2 event batches"
```

---

### Task 4: Plan limits from the Lab, delete engine pricing and tiers

**Files:**
- Modify: `crates/scrapix-auth/src/types.rs` (`Limits`, `AuthenticatedAccount.limits`)
- Modify: `bins/scrapix-api/src/lab_client.rs` (`Answer.limits`, `Identity.limits`, `FakeLab::identity`)
- Modify: `bins/scrapix-api/src/auth/middleware.rs:140-145,162-167,190-195,226-231` (`limits: id.limits`)
- Modify: `bins/scrapix-api/src/lib.rs:2614-2633` (`AccountContext`), `4555-4572` (crawl preflight), `3540-3553` (scrape pre-check), `5082-5085`, `5516-5519`; tests constructing `AccountContext` / `AuthenticatedAccount` (`8330-8336`, `8368-8373`, `8708-8713`)
- Modify: `bins/scrapix-api/src/engine_jobs.rs:40-46,59-81`
- Modify: `bins/scrapix-api/src/billing.rs` (whole file)
- Modify: `bins/scrapix-api/src/batch.rs:213-217,264-265,613`; `extract.rs:488-490,621-630,626`; `documents.rs:190-196,276-285,624-629,1052-1058`; `scrape_tests.rs:13-18`; `analytics_pipes.rs:428-433,579-584`; `diagnostics.rs:387-392`
- Delete: `crates/scrapix-billing/` (whole crate), `crates/scrapix-core/src/billing.rs`
- Modify: `Cargo.toml:13` (workspace member), `bins/scrapix-api/Cargo.toml:19`, `crates/scrapix-core/src/lib.rs:13,27`

**Interfaces:**
- Produces: `scrapix_auth::Limits { concurrent_jobs: i64, rate_limit_rpm: i64, max_depth: u32, js_rendering: bool }` (Deserialize, Clone, Debug, PartialEq, Eq); `AuthenticatedAccount.limits: Option<Limits>`; `AccountContext.limits: Option<Limits>`; `billing::check_credits(lab: &LabClient, account_id: &str) -> Result<i64, ApiError>`; `engine_jobs::PlanCheck { max_depth: Option<u32>, js_rendering: bool }`; `engine_jobs::enforce_limits(ctx: &AccountContext, active_jobs: i64, check: PlanCheck) -> Result<(), ApiError>`; `engine_jobs::preflight(state, account_ctx, check: PlanCheck)`.

- [ ] **Step 1: Write the failing tests**

`engine_jobs.rs` tests module (create one at the end of the file if none exists):

```rust
#[cfg(test)]
mod limit_tests {
    use super::*;
    use scrapix_auth::Limits;

    fn ctx(limits: Option<Limits>) -> AccountContext {
        AccountContext {
            account_id: "7f1c2a8e-0000-4000-8000-000000000001".into(),
            api_key_id: None,
            tier: "free".into(),
            user_role: None,
            limits,
        }
    }

    fn free() -> Limits {
        Limits { concurrent_jobs: 1, rate_limit_rpm: 60, max_depth: 3, js_rendering: false }
    }

    #[test]
    fn concurrent_jobs_max_depth_and_js_rendering_come_from_the_labs_limits() {
        let ok = PlanCheck { max_depth: Some(3), js_rendering: false };
        assert!(enforce_limits(&ctx(Some(free())), 0, ok.clone()).is_ok());
        let e = enforce_limits(&ctx(Some(free())), 1, ok.clone()).unwrap_err();
        assert_eq!(e.code, "quota_exceeded");
        assert!(e.error.contains("1/1"), "{}", e.error);
        let e = enforce_limits(&ctx(Some(free())), 0, PlanCheck { max_depth: Some(4), js_rendering: false })
            .unwrap_err();
        assert_eq!(e.code, "quota_exceeded");
        assert!(e.error.contains("max_depth"), "{}", e.error);
        let e = enforce_limits(&ctx(Some(free())), 0, PlanCheck { max_depth: None, js_rendering: true })
            .unwrap_err();
        assert_eq!(e.code, "quota_exceeded");
        assert!(e.error.contains("JS rendering"), "{}", e.error);
        let pro = Limits { concurrent_jobs: 10, rate_limit_rpm: 1200, max_depth: 10, js_rendering: true };
        assert!(enforce_limits(&ctx(Some(pro)), 9, PlanCheck { max_depth: Some(10), js_rendering: true }).is_ok());
    }

    #[test]
    fn missing_limits_skips_enforcement_with_one_warning() {
        // A Lab that predates `limits` (contract v1): nothing to enforce.
        assert!(enforce_limits(&ctx(None), 100, PlanCheck { max_depth: Some(99), js_rendering: true }).is_ok());
    }
}
```

`billing.rs` `lab_balance_tests`: replace the module with:

```rust
#[cfg(test)]
mod lab_balance_tests {
    use super::check_credits;
    use crate::lab_client::{
        testing::{FakeLab, INSTANCE_ID, SECRET},
        LabClient,
    };
    use serde_json::json;

    const ACCT: &str = "11111111-1111-1111-1111-111111111111";

    fn account(balance: i64) -> serde_json::Value {
        json!({"active": true, "account_id": ACCT, "tier": "free", "credits": {"balance": balance}})
    }

    #[tokio::test]
    async fn a_zero_or_negative_balance_is_402() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        lab.set_account(ACCT, account(0));
        assert_eq!(check_credits(&c, ACCT).await.unwrap_err().code, "insufficient_credits");
        lab.set_account(ACCT, account(-3));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // The refusal refreshes once, so the new negative balance is seen.
        assert_eq!(check_credits(&c, ACCT).await.unwrap_err().code, "insufficient_credits");
    }

    #[tokio::test]
    async fn a_top_up_counts_before_a_402_with_one_refresh_per_check() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        lab.set_account(ACCT, account(1));
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 1);
        let calls = lab.calls();
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 1);
        assert_eq!(lab.calls(), calls, "positive snapshot: no Lab call");
        lab.set_account(ACCT, account(0));
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 1, "snapshot still positive, served cached");
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        assert_eq!(check_credits(&c, ACCT).await.unwrap_err().code, "insufficient_credits");
        lab.set_account(ACCT, account(10));
        let calls = lab.calls();
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 10, "refreshed once before refusing");
        assert_eq!(lab.calls(), calls + 1);
    }

    #[tokio::test]
    async fn billing_unavailable_is_503_with_retry_after() {
        use axum::response::IntoResponse;
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        lab.set_down(true);
        let resp = check_credits(&c, ACCT).await.unwrap_err().into_response();
        assert_eq!(resp.status(), 503);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    }

    /// Spec 8.1: inside the stale window the last snapshot is served; past
    /// it, with the Lab still unreachable, the pre-check fails closed (503).
    /// This is today's `refresh_credits` behaviour, kept as is.
    #[tokio::test]
    async fn past_the_stale_window_with_the_lab_down_is_503() {
        use crate::lab_client::Timing;
        use std::time::Duration;
        let lab = FakeLab::start().await;
        let c = LabClient::with_timing(
            &lab.url,
            INSTANCE_ID,
            SECRET,
            Timing {
                default_ttl: Duration::from_millis(100),
                stale_grace: Duration::from_millis(100),
                stale_retry: Duration::from_millis(20),
                ..Timing::default()
            },
        );
        lab.set_account(ACCT, account(5));
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 5);
        lab.set_down(true);
        tokio::time::sleep(Duration::from_millis(120)).await; // past ttl, inside grace
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 5, "stale snapshot served");
        tokio::time::sleep(Duration::from_millis(150)).await; // past ttl + grace
        assert_eq!(check_credits(&c, ACCT).await.unwrap_err().code, "service_unavailable");
    }

    #[tokio::test]
    async fn unknown_account_is_not_found_and_lab_down_is_503() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        assert_eq!(check_credits(&c, ACCT).await.unwrap_err().code, "not_found");
        lab.set_down(true);
        let other = "22222222-2222-2222-2222-222222222222";
        assert_eq!(check_credits(&c, other).await.unwrap_err().code, "service_unavailable");
    }
}
```

`lab_client.rs`: add to `introspection_is_cached_until_ttl` after the tier assertion:

```rust
        assert_eq!(
            id.limits,
            Some(scrapix_auth::Limits { concurrent_jobs: 10, rate_limit_rpm: 1200, max_depth: 10, js_rendering: true })
        );
```

and change `FakeLab::identity` to include limits for the tier:

```rust
        pub(crate) fn identity(account: &str, tier: &str, balance: i64) -> Value {
            let limits = match tier {
                "pro" => json!({"concurrent_jobs": 10, "rate_limit_rpm": 1200, "max_depth": 10, "js_rendering": true}),
                _ => json!({"concurrent_jobs": 1, "rate_limit_rpm": 60, "max_depth": 3, "js_rendering": false}),
            };
            json!({"active": true, "account_id": account, "tier": tier, "role": null, "api_key_id": null,
                   "principal": {"type": "api_key", "user_id": null}, "credits": {"balance": balance},
                   "limits": limits})
        }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p scrapix-api limit_tests lab_balance_tests 2>&1 | head -20`
Expected: compile errors (`Limits`, `PlanCheck`, `enforce_limits`, `limits` field missing; `check_credits` arity).

- [ ] **Step 3: Add `Limits` and carry it through identity**

`crates/scrapix-auth/src/types.rs`:

```rust
/// Plan limits the Lab serves with every identity (platform contract v2 §5).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Limits {
    pub concurrent_jobs: i64,
    pub rate_limit_rpm: i64,
    pub max_depth: u32,
    pub js_rendering: bool,
}

/// The account a request acts as, whatever the credential (API key, OAuth
/// Bearer, session or service call).
#[derive(Debug, Clone)]
pub struct AuthenticatedAccount {
    pub account_id: String,
    pub tier: String,
    pub api_key_id: Option<String>,
    /// Member role for session/OAuth principals (`owner`/`admin`/`member`/`viewer`); None for API keys and service calls.
    pub role: Option<String>,
    /// `None` when the Lab did not send limits (contract v1).
    pub limits: Option<Limits>,
}
```

`crates/scrapix-auth/src/lib.rs`: export `Limits` (`pub use types::{AuthenticatedAccount, AuthenticatedUser, Limits};`). `bins/scrapix-api/src/auth/mod.rs:14`: `pub use scrapix_auth::{AuthenticatedAccount, Claims, Limits};`.

`lab_client.rs`: `Answer` gains `#[serde(default)] limits: Option<scrapix_auth::Limits>,`; `Identity` gains `pub limits: Option<scrapix_auth::Limits>,` and `to_identity` sets `limits: a.limits`. `middleware.rs`: every `AuthenticatedAccount { ... }` literal (140, 162, 190, 226) gets `limits: id.limits,` (move `id.limits` before any field that moves `id` partially: all four literals already move fields out of `id`, so adding `limits: id.limits` is fine). Test literals in `analytics_pipes.rs:579`, `diagnostics.rs:387`, `lib.rs:8708` get `limits: None,`.

`lib.rs` `AccountContext` gains `pub limits: Option<scrapix_auth::Limits>,` and `extract_account_context` copies `limits: acct.limits.clone(),`; `engine_jobs::clone_account_ctx` copies `limits: c.limits.clone(),`. Test literals (`lib.rs:8330`, `8368`, `documents.rs:1052`, `extract.rs:1293`, `scrape_tests.rs:13`, `analytics_pipes.rs:428`) get `limits: None,`.

- [ ] **Step 4: Replace the preflight**

`engine_jobs.rs` (replace lines 59-81):

```rust
/// What a job asks of the plan (the Lab's `limits`).
#[derive(Debug, Clone)]
pub(crate) struct PlanCheck {
    pub max_depth: Option<u32>,
    pub js_rendering: bool,
}

static LIMITS_MISSING_WARNED: std::sync::Once = std::sync::Once::new();

/// Enforce the Lab's plan limits: concurrent jobs, crawl depth and JS
/// rendering. A Lab that sent no `limits` enforces nothing (warned once).
pub(crate) fn enforce_limits(
    ctx: &AccountContext,
    active_jobs: i64,
    check: PlanCheck,
) -> Result<(), ApiError> {
    let Some(limits) = ctx.limits.as_ref() else {
        LIMITS_MISSING_WARNED.call_once(|| {
            tracing::warn!("The Lab sent no plan limits (contract v1?): concurrent jobs, max_depth and JS rendering are not enforced")
        });
        return Ok(());
    };
    if active_jobs >= limits.concurrent_jobs {
        return Err(ApiError::new(
            format!(
                "Maximum concurrent jobs reached ({}/{}). Upgrade your plan for more.",
                active_jobs, limits.concurrent_jobs
            ),
            "quota_exceeded",
        ));
    }
    if let Some(depth) = check.max_depth {
        if depth > limits.max_depth {
            return Err(ApiError::new(
                format!("max_depth {depth} exceeds your plan's limit of {}", limits.max_depth),
                "quota_exceeded",
            ));
        }
    }
    if check.js_rendering && !limits.js_rendering {
        return Err(ApiError::new(
            "JS rendering (browser crawler, render_js, screenshots, actions) is not included in your plan",
            "quota_exceeded",
        ));
    }
    Ok(())
}

/// Balance pre-check and plan limits, as for `/crawl` (hosted only).
pub(crate) async fn preflight(
    state: &AppState,
    account_ctx: &Option<AccountContext>,
    check: PlanCheck,
) -> Result<(), ApiError> {
    let (Some(lab), Some(ctx)) = (&state.lab_api, account_ctx) else {
        return Ok(());
    };
    billing::check_credits(lab, &ctx.account_id).await?;
    let active_count = state.active_job_count(&ctx.account_id).await;
    enforce_limits(ctx, active_count, check)
}
```

`lib.rs` crawl start (4555-4572) becomes:

```rust
    // Pre-flight (hosted): a positive balance and the plan's limits.
    engine_jobs::preflight(
        state,
        &account_ctx,
        engine_jobs::PlanCheck {
            max_depth: config.max_depth,
            js_rendering: config.crawler_type == CrawlerType::Browser,
        },
    )
    .await?;
```

(`account_ctx` here is `Option<AccountContext>` by value in that function: if it was moved into the `if let` before, bind `&account_ctx` as the existing code does and keep the later uses.) Scrape (3540-3553): delete the `scrape_cost` computation and replace the pre-flight with:

```rust
    if let (Some(ref lab), Some(ref ctx)) = (&state.lab_api, &account_ctx) {
        billing::check_credits(lab, &ctx.account_id).await?;
        let wants_browser = request.render_js
            || request.formats.contains(&ScrapeFormat::Screenshot)
            || !request.actions.is_empty()
            || request.mobile;
        engine_jobs::enforce_limits(ctx, 0, engine_jobs::PlanCheck { max_depth: None, js_rendering: wants_browser })?;
    }
```

Map (5082-5085) and search (5516-5519): `billing::check_credits(lab, &ctx.account_id).await?;`. `batch.rs`: delete `credits_per_url` (213-217) and its test assertion at 613; the preflight at 264-265 becomes `engine_jobs::preflight(state, account_ctx, engine_jobs::PlanCheck { max_depth: None, js_rendering: batch.sample.render_js }).await?;`. `extract.rs`: 488-490 becomes `engine_jobs::preflight(state, account_ctx, engine_jobs::PlanCheck { max_depth: None, js_rendering: render_js }).await?;` (delete the `globs`/`minimum` lines); `can_afford_ai` (621-630) calls `billing::check_credits(lab, &ctx.account_id)`. `documents.rs`: delete `DocumentJob::credits` (190-196); the OCR pre-flight (276-285) becomes `if let (Some(lab), Some(ctx)) = (&state.lab_api, account_ctx) { billing::check_credits(lab, &ctx.account_id).await?; }` and the upload pre-flight (624-629) the same (delete `base_cost`, `has_ai_summary`, `has_ai_extraction` locals if nothing else reads them; `require_ai_provider` still runs).

- [ ] **Step 5: Rewrite `billing.rs` and delete the pricing crates**

`bins/scrapix-api/src/billing.rs` becomes:

```rust
//! The hosted engine's balance pre-check (platform contract v2 §8). Prices
//! live in the Lab (`saas/config/pricing.yml`): the engine reports units
//! and only refuses to start billable work when the balance is gone.

use crate::ApiError;

#[derive(Debug)]
pub(crate) enum BillingError {
    InsufficientCredits { available: i64 },
    AccountNotFound,
}

impl BillingError {
    fn code(&self) -> &'static str {
        match self {
            BillingError::InsufficientCredits { .. } => "insufficient_credits",
            BillingError::AccountNotFound => "not_found",
        }
    }
}

impl std::fmt::Display for BillingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BillingError::InsufficientCredits { available } => {
                write!(f, "Insufficient credits: {available} available, top up to continue")
            }
            BillingError::AccountNotFound => f.write_str("Account not found or inactive"),
        }
    }
}

impl From<BillingError> for ApiError {
    fn from(e: BillingError) -> Self {
        ApiError::new(e.to_string(), e.code())
    }
}

/// Refuse when the Lab's balance for `account_id` is `<= 0`. A snapshot
/// that says so is refreshed once first, so a top-up made since counts.
/// While the Lab is unreachable the last snapshot is served for the stale
/// window (`Timing::stale_grace`, 300 s); past it the check is a 503 with
/// `Retry-After: 5` (spec §8.1), which is what `LabClient::refresh_credits`
/// already does today (`Err(LabError::Unavailable)` once no snapshot is
/// usable): this function only maps that error, it adds no new rule.
pub(crate) async fn check_credits(
    lab: &crate::lab_client::LabClient,
    account_id: &str,
) -> Result<i64, ApiError> {
    let answer = match lab.available_credits(account_id).await {
        Ok(Some(a)) if a.cached && a.credits <= 0 => lab.refresh_credits(account_id).await,
        other => other,
    };
    match answer.map(|a| a.map(|a| a.credits)) {
        Ok(Some(available)) if available > 0 => Ok(available),
        Ok(Some(available)) => Err(BillingError::InsufficientCredits { available }.into()),
        Ok(None) => Err(BillingError::AccountNotFound.into()),
        Err(e) => {
            crate::lab_client::log_lab_error(&e, "credit check");
            Err(ApiError::new(
                "Billing service unavailable, retry shortly",
                "service_unavailable",
            )
            .with_retry_after(5))
        }
    }
}
```

followed by the `lab_balance_tests` module from Step 1. `batch.rs:340` keeps matching `"insufficient_credits"`; the `"spend_limit_exceeded"` code is no longer produced but matching it is harmless (leave it).

Delete the crate and the core module:

```bash
git rm -r crates/scrapix-billing
git rm crates/scrapix-core/src/billing.rs
```

Remove `"crates/scrapix-billing",` from `Cargo.toml` members, `scrapix-billing = ...` from `bins/scrapix-api/Cargo.toml`, and `pub mod billing;` / `pub use billing::*;` from `crates/scrapix-core/src/lib.rs`. Run `cargo check --workspace` and delete any remaining `scrapix_billing::` reference it reports (expected: `lib.rs:1289` already removed in Task 1; `documents.rs:218,283,1090` removed above; `scrape_tests.rs` references `billing::scrape_credits` only inside the assertions rewritten in Task 1).

- [ ] **Step 6: Run the workspace tests**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 7: Pre-commit and commit**

```bash
cargo fmt && cargo check && cargo clippy
git add -A Cargo.toml Cargo.lock crates bins
git commit -m "feat(billing): enforce the Lab's plan limits and drop the engine's price table and tiers"
```

---

### Task 5: No operator Meilisearch for a hosted tenant

**Files:**
- Modify: `bins/scrapix-api/src/meili.rs:58-113,198-250,262-272` (`LabMeilisearchResolver`, `server_fallback`, tests)
- Modify: `bins/scrapix-api/src/results.rs:449-495` (`resolve_crawl_target`)
- Modify: `bins/scrapix-api/src/settings.rs` hosted branch (warn and drop `MEILISEARCH_URL`), `bins/scrapix-api/src/lib.rs:6768-6771` (resolver construction)

**Interfaces:**
- Produces: `LabMeilisearchResolver { lab: Arc<LabClient> }` (no `server`); `EngineSettings.meilisearch` is always `None` in hosted mode.

- [ ] **Step 1: Write the failing tests**

`meili.rs`: replace `lab_resolver_default_url_and_fallback` with:

```rust
    #[tokio::test]
    async fn hosted_resolver_never_falls_back_to_the_operator_server() {
        use crate::lab_client::{
            testing::{FakeLab, INSTANCE_ID, SECRET},
            LabClient,
        };
        let lab = FakeLab::start().await;
        let acct = "11111111-1111-1111-1111-111111111111";
        lab.state.meili.lock().unwrap().insert(
            format!("{acct}|"),
            serde_json::json!({"id": "e", "url": "http://m:7700", "api_key": "k"}),
        );
        let r = LabMeilisearchResolver {
            lab: std::sync::Arc::new(LabClient::new(&lab.url, INSTANCE_ID, SECRET)),
        };
        assert_eq!(r.default_target(Some(acct)).await.unwrap().unwrap().url, "http://m:7700");
        assert_eq!(r.default_target(None).await.unwrap(), None);
        assert_eq!(r.target_for_url(Some(acct), "http://ops:7700/").await.unwrap(), None, "a URL the Lab does not know is unknown");
        assert_eq!(r.target_for_url(None, "http://ops:7700").await.unwrap(), None);
        let other = "22222222-2222-2222-2222-222222222222";
        assert_eq!(r.default_target(Some(other)).await.unwrap(), None, "no target, no fallback");
        let err = resolve_crawl_meilisearch(&r, Some(other), Default::default()).await.unwrap_err();
        assert_eq!(err.code, "validation_error");
        assert!(err.error.contains("Settings"), "{}", err.error);
        lab.set_down(true);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let third = "33333333-3333-3333-3333-333333333333";
        let err = r.default_target(Some(third)).await.unwrap_err();
        let resp = axum::response::IntoResponse::into_response(err);
        assert_eq!(resp.status(), 503);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    }
```

Delete `server_fallback_only_matches_same_url`. `settings.rs` tests, add:

```rust
    #[test]
    fn hosted_ignores_the_operator_meilisearch() {
        let s = EngineSettings::resolve(&args(&hosted(&[
            ("MEILISEARCH_URL", "http://ops:7700"),
            ("MEILISEARCH_API_KEY", "ops"),
        ])))
        .unwrap();
        assert!(s.meilisearch.is_none(), "a tenant must never land in the operator's Meilisearch");
    }
```

`results.rs`: find the existing tests of `resolve_crawl_target` (search `resolve_crawl_target(` in the tests module) and add:

```rust
    #[tokio::test]
    async fn results_target_is_only_the_jobs_own() {
        let bus = ChannelBus::new();
        let state = test_support::test_state(&bus); // EnvResolver(None)
        let mut job = JobState::new_for_test("orphan"); // use the module's existing job constructor helper
        job.config = Some(serde_json::json!({"meilisearch": {"url": ""}}));
        let err = resolve_crawl_target(&state, &job).await.unwrap_err();
        assert_eq!(err.code, "not_found");
        job.config = Some(serde_json::json!({"meilisearch": {"url": "http://job:7700"}}));
        let t = resolve_crawl_target(&state, &job).await.unwrap();
        assert_eq!((t.url.as_str(), t.api_key), ("http://job:7700", None));
    }
```

Use the job constructor the existing `results.rs` tests use (search for `JobState {` or a `job(` helper in that module) in place of `JobState::new_for_test`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p scrapix-api meili results::tests settings::tests::hosted_ignores 2>&1 | head -20`
Expected: compile error (`server` field required) and the settings test fails (`meilisearch` is `Some`).

- [ ] **Step 3: Implement**

`meili.rs`: `LabMeilisearchResolver` keeps only `pub(crate) lab`; delete `server_fallback` and its doc; `target_for_url`:

```rust
    async fn target_for_url(
        &self,
        account_id: Option<&str>,
        url: &str,
    ) -> Result<Option<MeiliTarget>, ApiError> {
        let Some(account) = account_id else {
            return Ok(None);
        };
        self.lab.meilisearch(account, Some(url)).await.map_err(lab_err)
    }
```

Update the struct doc: "Hosted: the account's engines as the Lab reports them (`GET /internal/accounts/{id}/meilisearch`), and nothing else: a tenant's crawl never lands in the operator's Meilisearch." `lib.rs:6768-6771`: `meili: Arc::new(meili::LabMeilisearchResolver { lab: lab_api.clone() })`.

`settings.rs` hosted branch, before building `LabSettings`: 

```rust
                if meilisearch.is_some() {
                    tracing::warn!("MEILISEARCH_URL is ignored in hosted mode: tenants use the Meilisearch targets registered in the Lab");
                }
```

and `meilisearch: None,` in the hosted `Ok(Self { ... })`.

`results.rs` `resolve_crawl_target`: delete the trailing `match state.meili.default_target(account).await { ... }` and end with:

```rust
    Err(ApiError::new(
        "The Meilisearch instance of this job is unknown",
        "not_found",
    ))
```

Keep the `target_for_url` lookup and the "unknown key: try without one" step for the job's own config URL (that is the job's target, not the operator's).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scrapix-api 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Pre-commit and commit**

```bash
cargo fmt && cargo check && cargo clippy
git add bins/scrapix-api
git commit -m "fix(hosted): never fall back to the operator's Meilisearch for a tenant"
```

---

### Task 6: CI runs the tests, deploy manifests, LICENSE, Cargo repository, README and docs, `/parse` in the public spec

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `deploy/kubernetes/base/config/configmap.yaml:23-27,41`, `deploy/kubernetes/base/config/secrets.yaml:14-18`, `deploy/kubernetes/overlays/prod/kustomization.yaml:42`, `deploy/kubernetes/overlays/scaleway/patches/sealed-secrets.yaml:8-9,25-26`
- Create: `deploy/kubernetes/README.md`, `LICENSE`
- Modify: `Cargo.toml:34`, `README.md:36,47,49,56-96,614-627`
- Modify: `docs/architecture.mdx:37,39,137,140`, `docs/configuration/environment-variables.mdx:22-29,153,262-270`, `docs/api-reference/lab-events.mdx`, `docs/deployment/kubernetes.mdx:157-167`, `docs/deployment/docker-compose.mdx:103,148`, `docs/operations/crawl-engine-rollout.mdx:24,51-64`, `.env.example:28-42`, `docker-compose.yml:195-199`, `CLAUDE.md:114,284-323,624-625`
- Modify: `contracts/openapi.json` (`/parse` path, `ParseUpload` schema, `parse` tag), `sdks/typescript/src/generated/schema.ts`, `sdks/python/src/scrapix/_generated/models.py` (generated)

- [ ] **Step 1: CI runs `cargo test --workspace`**

Add after the `clippy` job in `.github/workflows/ci.yml`:

```yaml
  test:
    name: Tests
    runs-on: ubuntu-latest
    needs: [clippy]
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      # Postgres-backed outbox tests skip themselves without a database
      # (`test_pg_pool()` returns None); everything else runs here.
      - run: cargo test --workspace
```

Run `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))"` to validate the YAML (or `npx yaml-lint` if Python's yaml module is absent).

- [ ] **Step 2: Kubernetes base**

`configmap.yaml` lines 23-27 become:

```yaml
  # Lab (control plane) base URL (hosted mode; required). The API calls
  # {LAB_URL}/internal/* and reports events to {LAB_URL}/internal/events.
  # The Lab is deployed from meilisearch/lab, not from these manifests:
  # set this to its public URL.
  LAB_URL: "https://lab.meilisearch.com"
```

and line 41's user agent URL becomes `https://github.com/qdequele/scrapix`. `secrets.yaml` lines 14-18 become:

```yaml
  # REQUIRED in hosted mode (the API refuses to start without them).
  # LAB_INSTANCE_ID / LAB_INSTANCE_SECRET are minted by the Lab for this
  # engine (see deploy/kubernetes/README.md); LAB_SERVICE_TOKEN is the
  # token the Lab presents when it calls this engine (>= 32 chars, same
  # value on the Lab). Set them in your overlay or with kubectl.
  LAB_INSTANCE_ID: "CHANGE_ME"
  LAB_INSTANCE_SECRET: "CHANGE_ME"
  LAB_SERVICE_TOKEN: "CHANGE_ME"
```

`prod/kustomization.yaml:42` comment: `#       - secrets.env   # must include LAB_INSTANCE_ID, LAB_INSTANCE_SECRET, LAB_SERVICE_TOKEN`. `sealed-secrets.yaml`: replace the two `LAB_EVENTS_SECRET`/`LAB_SERVICE_TOKEN` `--from-literal` lines with

```
#     --from-literal=LAB_INSTANCE_ID=<from bin/rails lab:hosted_engine:create> \
#     --from-literal=LAB_INSTANCE_SECRET=<from bin/rails lab:hosted_engine:create> \
#     --from-literal=LAB_SERVICE_TOKEN=$(openssl rand -hex 32) \
```

and the `encryptedData` keys `LAB_EVENTS_SECRET` → `LAB_INSTANCE_ID`, plus a new `LAB_INSTANCE_SECRET: AgA...REPLACE_WITH_SEALED_VALUE` line (keep `LAB_SERVICE_TOKEN`).

Create `deploy/kubernetes/README.md`:

```markdown
# Kubernetes manifests (hosted engine)

`base/` deploys the Scrapix engine in **hosted** mode (`SCRAPIX_MODE=hosted`
on the API): it keeps its own `scrapix_engine` Postgres and talks to the
Lab (`meilisearch/lab`, deployed separately) over `LAB_URL`. To self-host
without the Lab, patch the API to `SCRAPIX_MODE=standalone` with
`SCRAPIX_ADMIN_KEY` and drop the `LAB_*` values (see
`docs/deployment/self-hosting.mdx`).

## Secrets the API needs

| Key | Where it comes from |
|-----|---------------------|
| `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET` | Minted by the Lab for this engine deployment: on the Lab, run `bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=<region> URL=<this API's public URL>`; it prints `instance_id`, `secret` (64 hex, shown once) and `lab_url`. Rotate with the same task; the old secret stays valid for 10 minutes. |
| `LAB_SERVICE_TOKEN` | `openssl rand -hex 32`; the same value on the Lab (`LAB_SERVICE_TOKEN`). The Lab presents it when it calls this engine for an account (saved-config cron). |
| `MEILISEARCH_API_KEY`, `POSTGRES_PASSWORD`, `CLICKHOUSE_PASSWORD` | Your infrastructure. |

`LAB_URL` lives in the ConfigMap (`base/config/configmap.yaml`): the Lab's
public base URL, the `lab_url` the mint task printed.

```bash
kubectl -n scrapix create secret generic scrapix-secrets \
  --from-literal=LAB_INSTANCE_ID=<instance_id> \
  --from-literal=LAB_INSTANCE_SECRET=<secret> \
  --from-literal=LAB_SERVICE_TOKEN=$(openssl rand -hex 32) \
  --from-literal=MEILISEARCH_API_KEY=... \
  --from-literal=POSTGRES_PASSWORD=... \
  --from-literal=CLICKHOUSE_PASSWORD=...
```

The API refuses to start when `LAB_INSTANCE_ID` is not a uuid, when
`LAB_INSTANCE_SECRET` is not 64 hex characters, when the Lab rejects them,
or when `GET {LAB_URL}/internal/instances/me` is missing (a Lab that predates
contract v2 or a wrong `LAB_URL`).
```

Validate with `kubectl kustomize deploy/kubernetes/base > /dev/null` if `kubectl` is installed (otherwise `python3 -c "import yaml; [yaml.safe_load(open(f)) for f in ['deploy/kubernetes/base/config/configmap.yaml','deploy/kubernetes/base/config/secrets.yaml']]"`).

- [ ] **Step 3: LICENSE and Cargo repository**

Create `LICENSE` (first commit in this repo is from 2023):

```
MIT License

Copyright (c) 2023-2026 Quentin de Quelen

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

`Cargo.toml:34`: `repository = "https://github.com/qdequele/scrapix"`. Run `grep -rn "quentindequelen/scrapix" --exclude-dir=target --exclude-dir=node_modules .` and replace every hit with `qdequele/scrapix` (docs links, configmap user agent, `docs/api-reference/lab-events.mdx`).

- [ ] **Step 4: README**

- Line 36 (data layer diagram): `│  Redpanda │ Meilisearch │ DragonflyDB │ ClickHouse │ Postgres/SQLite │`. Delete the `Local State | RocksDB` and `Object Storage | S3/MinIO/RustFS` rows (47, 49) and add `| Job store | SQLite (default) or Postgres, engine-owned |`.
- Quick start (56-96): replace steps 1-3 with:

````markdown
### 1. Standalone in Docker (simplest)

```bash
cp .env.example .env            # SCRAPIX_MODE=standalone, SCRAPIX_ADMIN_KEY=...
docker compose -f compose.standalone.yaml up -d
curl -H "Authorization: Bearer $SCRAPIX_ADMIN_KEY" localhost:8080/health
```

`compose.standalone.yaml` starts Redpanda, Meilisearch and the engine
(`scrapix-api` + workers) with one operator key. `SCRAPIX_MODE=standalone`
is the default; `SCRAPIX_ADMIN_KEY` (16+ chars) is required unless
`SCRAPIX_AUTH=disabled` (local dev only).

### 2. Natively (iterating on the code)

```bash
docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d   # infra only
cargo build --release
```

In separate terminals:

```bash
# Terminal 1: API Server (standalone, operator key)
SCRAPIX_MODE=standalone \
SCRAPIX_ADMIN_KEY=dev-admin-key-change-me \
KAFKA_BROKERS=localhost:19092 \
MEILISEARCH_URL=http://localhost:7700 \
MEILISEARCH_API_KEY=masterKey \
cargo run --release --bin scrapix-api
```

(keep the frontier, crawler and content worker terminals as they are)

A standalone engine never talks to the Meilisearch Lab. Hosted deployments
(`SCRAPIX_MODE=hosted`, operated by Meilisearch) are documented in
`docs/deployment/kubernetes.mdx`.
````

- Lines 614-627: replace `- [Architecture](ARCHITECTURE.md)` with `- [Architecture](docs/architecture.mdx)` and `├── ARCHITECTURE.md` with `├── docs/                      # Mintlify docs site`; under "## License" write `MIT: see [LICENSE](LICENSE).`

- [ ] **Step 5: Docs**

- `docs/architecture.mdx:37`: Crawler stateful column → `No (robots.txt and DNS caches are in-process; `REDIS_URL` shares incremental-crawl headers)`; line 39 → `In-process channel bus (no redelivery, so at-least-once delivery only holds over Kafka)`; delete the table rows at 137 (RocksDB) and 140 (S3-compatible).
- `docs/configuration/environment-variables.mdx`: delete the `ROCKSDB_PATH` row (153) and the whole "Object Storage (S3-compatible)" section (262-270). Replace the Lab boundary paragraph (22) and rows (26-29) with:

```markdown
In hosted mode the engine reports usage and job lifecycle to the Lab ([`meilisearch/lab`](https://github.com/meilisearch/lab)) as signed events and calls the Lab's `/internal/*` API (credential introspection, balance and plan limits, Meilisearch targets): see [Lab Events](/api-reference/lab-events). Every call carries the engine's instance identity. A standalone engine never talks to a Lab: `LAB_INSTANCE_ID`/`LAB_INSTANCE_SECRET` set on a standalone engine are refused at startup.

| Variable | Where | Default | Description |
|----------|-------|---------|-------------|
| `LAB_URL` | engine | none | **Required (hosted).** The Lab's base URL, e.g. `https://lab.meilisearch.com` (no path). Events go to `{LAB_URL}/internal/events`; the rest of the boundary to `{LAB_URL}/internal/*`. |
| `LAB_INSTANCE_ID` | engine | none | **Required (hosted).** The uuid the Lab's operator minted for this deployment (`bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=... URL=...`). Sent as `X-Lab-Instance-Id`. Refused in standalone mode. |
| `LAB_INSTANCE_SECRET` | engine | none | **Required with `LAB_INSTANCE_ID`.** 64 hex characters minted with the id. Bearer token on `/internal/*` calls, HMAC key on event batches. Rotating it in the Lab keeps the old value valid for 10 minutes. |
| `LAB_SERVICE_TOKEN` | engine and Lab | none | **Required (hosted).** The token the Lab presents (with `X-Scrapix-Account-Id`) when it calls this engine for an account; at least 32 characters, identical on both sides. Never sent to the Lab. |
| `LAB_EVENTS_URL` | engine | none | **Deprecated.** Fallback for `LAB_URL` (the old `…/internal/events` URL). |
| `LAB_EVENTS_SECRET` | engine | none | **Deprecated, ignored** (warned at boot): batches are signed with `LAB_INSTANCE_SECRET`. |
```

- `docs/api-reference/lab-events.mdx`: rewrite the schema paragraph to name `contracts/vendor/lab/lab-events.schema.json` as a vendored copy of the Lab-owned file; the Note: "Hosted mode only. A standalone engine emits no lab events and refuses `LAB_INSTANCE_ID`/`LAB_INSTANCE_SECRET`."; `product` row: `"scrapix"` for this engine (`lumen`, `glutony` for the other engines); replace the `usage.recorded` table with:

```markdown
| `data` field | Type | Description |
|--------------|------|-------------|
| `operation` | string | `scrape`, `map`, `search`, `parse`, `ocr`, `extract` or `crawl`. |
| `units` | object of integer ≥ 0 | Raw units the Lab prices (`pages_http`, `pages_browser`, `requests`, `documents`) plus informational counters it stores and ignores (`ai_summary`, `ai_extraction`, `urls_found`, `results`, `pages_ai`, `pages_ocr`). |
| `provider_cost_micro_usd` | integer ≥ 0 | Upstream provider money the engine paid, in micro-USD. Scrapix reports `0` today. |
| `description` | string | Text shown on the ledger row. |
| `job_id` | string, optional | Present for job-scoped usage (a `crawl`, or an `extract` job). |

Per operation: `scrape` `{pages_http | pages_browser: 1, ai_summary, ai_extraction}`; `map` `{requests: 1, urls_found}`; `search` `{requests: 1, results}`; `parse` `{documents: 1}`; `ocr` `{documents: <recognized pages>, pages_ocr}`; `extract` `{documents: 1}` per AI call (globs also emit a `map`); `crawl` `{pages_http, pages_browser, pages_ai, pages_ocr}` once per terminal job. The Lab converts units to credits with its `pricing.yml`; the engine holds no price table.
```

  Delivery section:

```http
POST {LAB_URL}/internal/events
Content-Type: application/json
X-Lab-Instance-Id: <LAB_INSTANCE_ID>
X-Lab-Timestamp: <unix seconds>
X-Lab-Signature: sha256=<hex hmac-sha256("<X-Lab-Timestamp>.<raw body>", LAB_INSTANCE_SECRET)>

{"events": [ { ...event... }, { ...event... } ]}
```

  Bullets: batch up to 500; signature covers `"<timestamp>.<body>"`, the Lab rejects a timestamp more than 300 s from its clock; an event never acknowledged for 24 h is dropped with an error log and counted as `scrapix_lab_events_delivered_total{outcome="dropped"}`; the `401` row means "missing or wrong instance id / signature, or a revoked instance". Replace the Ruby/Shell verification snippets with the timestamped form (`"#{ts}.#{request.raw_post}"`, `printf '%s.' "$ts" | cat - body.json | openssl dgst -sha256 -hmac "$LAB_INSTANCE_SECRET" -hex`). "Lab internal API" section: `X-Lab-Instance-Id` + `Authorization: Bearer <LAB_INSTANCE_SECRET>`; add a row `GET /internal/instances/me | Called once at boot to confirm the credentials and log the deployment's product and region. A 401 or 404 aborts startup.`; note that introspect/account answers carry `limits` (`concurrent_jobs`, `rate_limit_rpm`, `max_depth`, `js_rendering`) which the engine enforces. Configuration table: `LAB_URL`, `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET`, `LAB_SERVICE_TOKEN` (inbound only), `LAB_EVENTS_URL` (deprecated), `LAB_EVENTS_SECRET` (deprecated, ignored). Last paragraph: "The hosted engine refuses to start without `LAB_URL`, `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET` and `LAB_SERVICE_TOKEN`."
- `docs/deployment/kubernetes.mdx:157-167`: the `kubectl create secret` block gets `LAB_INSTANCE_ID`/`LAB_INSTANCE_SECRET` (from the Lab's `lab:hosted_engine:create`) and `LAB_SERVICE_TOKEN`; the paragraph: "needs `LAB_URL` (the Lab's public base URL, in the `scrapix-config` ConfigMap) and refuses to start without `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET` and `LAB_SERVICE_TOKEN`; see `deploy/kubernetes/README.md`". Remove the `lab-saas` mention.
- `docs/deployment/docker-compose.mdx:103`: `` `SCRAPIX_MODE=hosted`, `LAB_URL`, `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET`, `LAB_SERVICE_TOKEN` ``; line 148: "In hosted mode, also set `LAB_INSTANCE_ID`/`LAB_INSTANCE_SECRET` (minted by the Lab) and `LAB_SERVICE_TOKEN` (the Lab's token for calling the engine)".
- `docs/operations/crawl-engine-rollout.mdx:24,51-64`: same substitution (`LAB_INSTANCE_ID`/`LAB_INSTANCE_SECRET` come from the Lab, not `openssl`; `LAB_SERVICE_TOKEN` keeps `openssl rand -hex 32`).
- `.env.example:28-42` hosted block:

```
# Run the Lab from a meilisearch/lab checkout (`just dev` there: Rails on
# :8081, Postgres on :5433 with a `scrapix_engine` database). Mint this
# engine's credentials on the Lab (`bin/rails lab:hosted_engine:create
# PRODUCT=scrapix REGION=dev URL=http://localhost:8080`), then replace the
# two standalone lines above with the block below. The engine's DATABASE_URL
# is its own database, never the Lab's.
# SCRAPIX_MODE=hosted
# LAB_URL=http://localhost:8081
# LAB_INSTANCE_ID=<uuid printed by the Lab>
# LAB_INSTANCE_SECRET=<64 hex printed by the Lab>
# LAB_SERVICE_TOKEN=dev-lab-service-token-change-in-production-0000
# DATABASE_URL=postgres://scrapix:scrapix@localhost:5433/scrapix_engine
#
# A standalone engine has no Lab: LAB_INSTANCE_ID / LAB_INSTANCE_SECRET are
# refused with SCRAPIX_MODE=standalone.
```

  `docker-compose.yml:195-199`: the commented hosted block gets `LAB_INSTANCE_ID`/`LAB_INSTANCE_SECRET` placeholders in place of `LAB_EVENTS_SECRET`.
- `CLAUDE.md`: lines 114, 284-323 and 624-625: replace `LAB_EVENTS_SECRET` + `LAB_SERVICE_TOKEN` (engine-to-Lab) with `LAB_INSTANCE_ID` + `LAB_INSTANCE_SECRET`; `contracts/lab-events.schema.json` becomes "vendored at `contracts/vendor/lab/lab-events.schema.json` (Lab-owned)"; the sentence "The engine owns ... `contracts/lab-events.schema.json`" drops that file; table rows 624-625 become `LAB_INSTANCE_ID` / `LAB_INSTANCE_SECRET` (hosted only, required; refused in standalone) and `LAB_SERVICE_TOKEN` (hosted only, inbound only).

- [ ] **Step 6: `/parse` in the public spec and the SDKs**

In `contracts/openapi.json`, before the `"/scrape": {` path (line 2139) insert the `/parse` object from `contracts/openapi.engine.json:1819-1869` verbatim (the `post` with `operationId: parse_upload`, `multipart/form-data` → `#/components/schemas/ParseUpload`, responses 200 `ScrapeResponse`, 400 `ApiError`, 413). In `components.schemas` insert the `ParseUpload` schema from `openapi.engine.json:4407-4434` verbatim, alphabetically before `ParserOptions` (line 7626). In the `tags` array add after `scrape`:

```json
    {
      "name": "parse",
      "description": "Document parsing (uploads)"
    },
```

Then:

```bash
python3 -c "import json; json.load(open('contracts/openapi.json'))"
just sdk-generate
git status --short sdks/
just sdk-check
```

Expected: `sdk-check` passes; the generated `schema.ts` / `models.py` gain `ParseUpload`. Run `cd sdks/typescript && npm run typecheck` and `cd sdks/python && uv run --group dev pytest -q` to confirm the hand-written clients still compile.

- [ ] **Step 7: Commit**

```bash
cargo fmt && cargo check && cargo clippy
git add -A .github deploy LICENSE Cargo.toml README.md docs .env.example docker-compose.yml CLAUDE.md contracts/openapi.json sdks
git commit -m "chore: run tests in CI, hosted deploy secrets for contract v2, LICENSE, docs and /parse in the public spec"
```

---

### Task 7: Dead code: `scrapix-auth` modules, stale allows, CLI Lab-only commands

**Files:**
- Delete: `crates/scrapix-auth/src/jwt.rs`, `password.rs`, `rate_limit.rs`
- Modify: `crates/scrapix-auth/src/lib.rs`, `crates/scrapix-auth/Cargo.toml`, `bins/scrapix-api/src/auth/mod.rs:8,13-14`
- Modify: `bins/scrapix-api/src/lab_events.rs` (any `#[allow(dead_code)]` left after Task 1; verify with `grep -n allow bins/scrapix-api/src/lab_events.rs`, expected: none)
- Delete: `bins/scrapix-cli/src/commands/team.rs`
- Modify: `bins/scrapix-cli/src/commands/mod.rs` (drop `pub mod team;`), `bins/scrapix-cli/src/lib.rs:52-58,255-259,439-451,644-661,828-838`, `bins/scrapix-cli/src/commands/auth.rs` (OAuth removal), `bins/scrapix-cli/src/config.rs`, `bins/scrapix-cli/src/types.rs:397-435`, `bins/scrapix-cli/Cargo.toml`

- [ ] **Step 1: `scrapix-auth` keeps only the shared types**

```bash
git rm crates/scrapix-auth/src/jwt.rs crates/scrapix-auth/src/password.rs crates/scrapix-auth/src/rate_limit.rs
```

`crates/scrapix-auth/src/lib.rs`:

```rust
//! Scrapix Auth
//!
//! The account/identity types the engine attaches to a request. Credential
//! verification lives in the Lab; the engine only resolves answers.

pub mod types;

pub use types::{AuthenticatedAccount, AuthenticatedUser, Limits};
```

`crates/scrapix-auth/Cargo.toml` dependencies: keep `uuid`, `serde` (derive); drop `jsonwebtoken`, `argon2`, `chrono`, `dashmap`; description `"Shared identity types for Scrapix"`. `bins/scrapix-api/src/auth/mod.rs:14`: `pub use scrapix_auth::{AuthenticatedAccount, Limits};` and line 8 doc: "Core identity types are in `scrapix-auth`." Run `grep -rn "Claims" bins/scrapix-api/src`: expected: no hits (if any, delete the import).

- [ ] **Step 2: CLI: remove `team` and the OAuth browser login**

```bash
git rm bins/scrapix-cli/src/commands/team.rs
```

- `commands/mod.rs`: remove `pub mod team;`.
- `lib.rs`: `Login` becomes `/// Store an API key for this CLI (prompted, hidden)` with no fields; delete the `Team { ... }` variant (255-259), the `TeamAction` enum (439-451), the `Commands::Team` arm (828-838); the auth resolution (644-652) becomes `let auth = cli.api_key.as_ref().map(|k| config::AuthCredential::ApiKey(k.clone())).or_else(|| cfg.auth_credential());` and `cfg` no longer needs `mut`; dispatch `Commands::Login => commands::auth::handle_login(&api_url).await,`.
- `types.rs`: delete `TeamMember`, `InviteMemberRequest`, `UpdateRoleRequest`, `TeamMemberRow` (397-435).
- `config.rs`: `CliConfig` keeps `api_url`, `api_key`, `output`; delete the four OAuth fields, `has_valid_token`, and `AuthCredential::Bearer`; `auth_credential` becomes `self.api_key.as_ref().map(|k| AuthCredential::ApiKey(k.clone()))`.
- `client.rs:33-35`: delete the `Bearer` arm.
- `auth.rs`: delete the OAuth types, PKCE helpers, `start_callback_server`, `parse_query_params`, `handle_login_oauth`, `refresh_token_if_needed`; `handle_login(api_url: &str)` is the former `handle_login_api_key` body minus the four `config.access_token = None`-style lines; `handle_logout` becomes `CliConfig::clear()?; print_success("Logged out. Credentials removed."); Ok(())`; `handle_status_auth`: `auth_method` is `"api_key"` or `"none"`, drop `"token_valid"` from the JSON and the OAuth branch of the text output. Remove now-unused imports (`HashMap`, `Arc`, `URL_SAFE_NO_PAD`, `Engine`, `Rng`, `Digest`, `Sha256`, `oneshot`, `print_error`, `print_info` if unused).
- `bins/scrapix-cli/Cargo.toml`: remove `open`, `rand`, `sha2`, `base64` (`urlencoding` stays: `analytics.rs:135`, `diagnostics.rs:214`); run `cargo check -p scrapix-cli` and restore any of the four the compiler reports as still imported elsewhere in the CLI.

- [ ] **Step 3: Verify**

```bash
cargo fmt && cargo check --workspace && cargo clippy --all -- -D warnings
cargo test --workspace 2>&1 | tail -10
cargo run -p scrapix-cli -- --help | grep -c "team"   # expected: 0
```

- [ ] **Step 4: Commit**

```bash
git add -A crates/scrapix-auth bins/scrapix-api/src/auth bins/scrapix-cli Cargo.lock
git commit -m "chore: remove unused auth primitives, the CLI team commands and the OAuth browser login"
```

---

## Self-review

**Spec coverage.** §2.A/B (hosted-only engines, Lab price table): Task 1 (units), Task 2 (no Lab in standalone), Task 4 (no prices/tiers, 1-credit pre-check, 503 past the stale window). §2.C (Lab-owned schema, vendored + drift-checked): Task 1. §2.D / §3.2-3.3 (per-instance credentials, headers, timestamped HMAC): Tasks 2-3. §3.4 headers and §3.5 (never-acknowledged event dropped after 24 h; the product-mismatch skip is Lab-side): Task 3. §3.6 (`instances/me` at boot, 401 aborts): Task 3. §4 (envelope, units, operations, job events unchanged): Task 1. §5 limits served by the Lab and §8 enforcement (concurrent jobs, max_depth, js_rendering, 30 s / 300 s cache): Task 4 (cache timing unchanged in `lab_client.rs`). §6 (security scheme) is Lab-side; the engine's vendored `lab-internal.openapi.json` is re-synced by `just sync-contracts` once the Lab publishes it. §9 transition: `LAB_EVENTS_SECRET` accepted-and-warned (Task 2), `credits` not sent (Task 1). Audit items: operator-Meilisearch fallback (Task 5), CI tests, k8s, LICENSE, repository URL, README, RocksDB/S3 docs, `/parse` (Task 6), dead code and CLI (Task 7). Not covered on purpose: `rate_limit_rpm` is carried in `Limits` but not enforced in the engine (the spec lists it as a served limit; the engine has no per-account rate limiter, and adding one is out of this plan's scope: the Lab proxy enforces `LAB_PROXY_RATE_LIMIT`).

**Placeholder scan.** No "TBD"/"TODO". One step deliberately defers a detail to the executor's reading of code the plan quotes by location rather than text: Task 5 Step 1's `results.rs` job constructor; it names the file, the function and the assertion. Task 7 Step 2 names the four dependencies and the compiler check that decides them.

**Type consistency.** `LabEvent::usage` (6 args) and `crawl_final_usage` (4 args) are used with those arities in Tasks 1 and 3. `LabClient::new(base, instance_id, secret)` in Tasks 3-5. `LabError::CredentialsRejected` in Task 3. `InstanceInfo` (`kind: String`, no account) in Task 3 only. `Limits` (scrapix-auth) with `concurrent_jobs: i64, rate_limit_rpm: i64, max_depth: u32, js_rendering: bool` in Task 4; `PlanCheck { max_depth: Option<u32>, js_rendering: bool }` matches `CrawlConfig.max_depth: Option<u32>`. `LabOutbox::abandon` in Task 3 across the three impls and the `TerminalStore` test wrapper. `check_credits(lab, account_id)` in Task 4 across `lib.rs`, `documents.rs`, `extract.rs`, `engine_jobs.rs`.

**Review Focus.** 1 → Task 3 `a_404_on_instances_me_aborts_startup`, Task 4 `missing_limits_skips_enforcement_with_one_warning`. 2 → Task 3 `events_older_than_24h_are_dropped_with_an_error`. 3 → Task 3 `signature_covers_timestamp_dot_body`. 4 → Task 2 `standalone_refuses_instance_credentials`. 5 → Task 5 `hosted_resolver_never_falls_back_to_the_operator_server`, `results_target_is_only_the_jobs_own`, `hosted_ignores_the_operator_meilisearch`.
