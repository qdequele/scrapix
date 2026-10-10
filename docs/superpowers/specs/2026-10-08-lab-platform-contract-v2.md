# Lab platform contract v2: hosted engines, Lab-owned events, Lab-owned pricing

- Date: 2026-10-08, revised 2026-10-09 (hosted-only)
- Status: accepted (decisions A, B, C, D, E taken on 2026-10-08 and 2026-10-09)
- Owner: `meilisearch/lab` (this document and the files it names are the source of truth; engines vendor them)
- Supersedes: Scrapix-owned `contracts/lab-events.schema.json`, the global `LAB_SERVICE_TOKEN` / `LAB_EVENTS_SECRET` pair in the engine-to-Lab direction, and the bring-your-own (BYO) engine model of the Gateway and Pipelines sections

## 1. Context

Four repos form one product line:

| Repo | Role | Lab kind / `product` |
|------|------|----------------------|
| `meilisearch/lab` (Rails `saas/` + Next.js `console/`) | Control plane: accounts, keys, credits, Stripe, instance registry, proxy | n/a |
| `qdequele/scrapix` | Crawler engine (OSS) | `scrapix` |
| `qdequele/glutony` (crates named `meili-ingest`) | Ingestion pipelines (OSS) | `glutony` |
| `qdequele/lumen` | LLM gateway (OSS) | `lumen` |

Audit on 2026-10-08 found: (1) the Lab receiver only accepts Scrapix's signature header and payload, so Lumen and glutony events are rejected with 401 forever; (2) there is no instance identity: one global `LAB_SERVICE_TOKEN` and one global `LAB_EVENTS_SECRET`, events keyed by `account_id` only; (3) pricing and plan limits live in the engines; (4) the Gateway and Pipelines sections only know customer-run engines, which the product does not want.

## 2. Decisions

- **A. Engines are hosted by Meilisearch only.** Lab users never register their own Scrapix, Lumen or glutony. Every account gets one built-in row per product, served by a deployment Meilisearch operates (`hosted_engines`). All engine usage is therefore billed. The BYO code paths that exist today (Gateway and Pipelines "Add instance" with URL, service token, edge secret) stay in the code behind `LAB_BYO_ENGINES=true` (default off) for the owner's own testing; they are not a product feature.
- **B. Price table in the Lab.** Engines report raw units and pass-through provider cost. The Lab converts to credits with `saas/config/pricing.yml`. Plan limits come from the Lab (`saas/config/plans.yml`) through the introspect and account endpoints; engines keep no tier constants.
- **C. One events contract, owned by the Lab.** `contracts/lab-events.schema.json` moves to the Lab. Each engine vendors a byte copy under `contracts/vendor/lab/` and drift-checks it in CI.
- **D. Per-deployment credentials.** Every hosted engine deployment authenticates to the Lab with its own id + secret, minted by the operator. The global pair is removed in the engine-to-Lab direction after a transition release.
- **E. Meilisearch instances stay user-registered.** Self-hosted (URL + key, as today) and, later, Meilisearch Cloud projects through the personal token and the Platform API (out of scope for this release, see §10). Nothing in this spec changes the Meilisearch section.
- **F. Hosted Lumen runs on Meilisearch's provider keys.** Users do not enter OpenAI/Anthropic/Cohere keys; provider cost is passed through as `provider_cost_micro_usd` and billed with the markup in `pricing.yml`.
- **G. One deployment per product at the start.** `hosted_engines.region` exists from day one; the Lab routes every account to the single active engine of each product. Per-account region choice comes later.

## 3. Identity model

### 3.1 Hosted engines

`hosted_engines` (new, platform-level, no `account_id`):

| Column | Notes |
|--------|-------|
| `id uuid` | The `LAB_INSTANCE_ID` the deployment presents |
| `product text` | `scrapix`, `lumen`, `glutony` |
| `region text` | e.g. `eu-west-1`; informational until G is revisited |
| `url text` | Where Rails reaches the engine (replaces `SCRAPIX_ENGINE_URL` for Scrapix) |
| `credential text` (encrypted) | Lab → engine platform credential: Scrapix `LAB_SERVICE_TOKEN` (inbound platform path, unchanged), Lumen master key, glutony `LAB_SERVICE_TOKEN` |
| `edge_secret text` (encrypted, nullable) | glutony `ENVOY_TRUSTED_HEADER` for ingest |
| `lab_secret`, `previous_lab_secret`, `previous_lab_secret_expires_at`, `lab_credentials_created_at` | Engine → Lab secret (`LAB_INSTANCE_SECRET`), rotation grace 10 minutes |
| `status text` | `active`, `revoked` |
| `created_at`, `updated_at` | |

Exactly one `active` engine per product is expected in this release; `HostedEngine.active_for(product)` returns it (the most recently created when there are several).

Operator-managed with rake tasks: `bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=eu-west-1 URL=https://... CREDENTIAL=... [EDGE_SECRET=...]` prints `LAB_URL`, `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET` once; `lab:hosted_engine:rotate ID=`; `lab:hosted_engine:revoke ID=`. No admin UI in this release.

### 3.2 Built-in instance rows

`instances` keeps serving the console. For each product, `Instance.hosted_for!(account_id, kind)` finds or creates the account's built-in row (`builtin: true`, slug `hosted`, `url` and `credential` empty) and links it to `HostedEngine.active_for(kind)` through `instances.hosted_engine_id`. Today's `hosted_scrapix_for!` becomes `hosted_for!(account_id, "scrapix")`.

For a built-in row, every Lab → engine call (`InstanceHttp`, `InstanceAuth`, `InstanceHealth`, the proxy, the Playgrounds, the saved-config cron) resolves `url`, `credential` and `edge_secret` from the linked hosted engine, and always scopes the call to the row's account:

| Product | Scoping headers on Lab → engine calls |
|---------|---------------------------------------|
| Scrapix | `Authorization: Bearer <credential>` + `X-Scrapix-Account-Id: <account>` (exists) |
| glutony | `Authorization: Bearer <credential>` + `X-Glutony-Tenant-Id: <account>` (exists); ingest adds `X-Meili-Tenant-Id` + `X-Meili-Envoy-Secret` (exists) |
| Lumen | `Authorization: Bearer <master key>` + `X-Lumen-Account-Ref: <account>` (**new**, see §8.3) |

User-registered rows of kind `lumen` or `glutony` are refused (`403 byo_disabled`) unless `LAB_BYO_ENGINES=true`. Kind `scrapix` was never user-registrable. Kind `meilisearch` is unchanged.

### 3.3 Engine configuration

Identical names in all three engines:

```
LAB_URL=https://lab.meilisearch.com      # existing
LAB_INSTANCE_ID=<uuid>                   # new
LAB_INSTANCE_SECRET=<64 hex>             # new
```

`LAB_SERVICE_TOKEN` and `LAB_EVENTS_SECRET` are accepted for one more release in the engine → Lab direction (engines warn at boot) and then removed there. Scrapix and glutony keep `LAB_SERVICE_TOKEN` as the inbound platform credential the Lab presents to them (unchanged). The Lab keeps accepting the legacy pair while `LAB_LEGACY_SERVICE_TOKEN` / `LAB_LEGACY_EVENTS_SECRET` are set, attributing them to `LAB_LEGACY_HOSTED_ENGINE_ID`; unset all three to retire them.

There is no "lab-connected standalone" mode: an engine either runs hosted for the Lab (the three variables set) or standalone without any Lab.

### 3.4 Authentication, engine to Lab

Every engine-to-Lab request carries:

```
X-Lab-Instance-Id: <LAB_INSTANCE_ID>
```

Service calls (`/internal/ping`, `/internal/auth/introspect`, `/internal/accounts/*`, `/internal/instances/me`):

```
Authorization: Bearer <LAB_INSTANCE_SECRET>
```

Event batches (`POST /internal/events`):

```
X-Lab-Timestamp: <unix seconds, integer>
X-Lab-Signature: sha256=<hex HMAC-SHA256(LAB_INSTANCE_SECRET, "<X-Lab-Timestamp>.<raw body>")>
```

The Lab resolves the id in `hosted_engines` (status `active`), compares the secret in constant time (current secret, then the previous one inside its 10-minute grace), and rejects a timestamp more than 300 s from its clock. A missing `X-Lab-Instance-Id` with a legacy `X-Scrapix-Signature` is verified against `LAB_LEGACY_EVENTS_SECRET` and attributed to `LAB_LEGACY_HOSTED_ENGINE_ID` (transition only).

### 3.5 Scoping rules

- A hosted engine may call for any active account. An event whose `product` differs from the engine's is skipped (`product mismatch`, never acknowledged; engines drop never-acknowledged events after 24 h with an error log).
- A revoked engine gets 401 on everything.

### 3.6 Self description

`GET /internal/instances/me` (Bearer secret + instance id) returns:

```json
{ "instance_id": "…", "kind": "hosted", "product": "scrapix", "region": "eu-west-1", "lab_url": "https://lab.meilisearch.com" }
```

Engines call it at boot to confirm their credentials and log their identity; a 401 aborts boot.

## 4. Events contract

File: `meilisearch/lab` `contracts/lab-events.schema.json` (JSON Schema 2020-12). Delivered as `{"events": [<event>, …]}`, at most 500 per batch. Response `200 {"accepted": ["<id>", …]}`; an id missing from `accepted` was skipped and is logged by the Lab with its reason.

### 4.1 Envelope

| Field | Type | Notes |
|-------|------|-------|
| `id` | uuid | Idempotency key. UUIDv7 per request; UUIDv5 of a stable name for job-final events (unchanged from today). |
| `type` | enum | `usage.recorded`, `job.completed`, `job.failed` |
| `occurred_at` | date-time | |
| `account_id` | uuid | The Lab account the usage belongs to |
| `api_key_id` | string or null | The Lab API key id when known |
| `product` | enum | `scrapix`, `lumen`, `glutony` |
| `data` | object | Per type, below |

`instance_id` is **not** in the body: the Lab takes it from the authenticated `X-Lab-Instance-Id` header and stores it on the row.

### 4.2 `usage.recorded` data (all products)

```json
{
  "operation": "crawl",
  "units": { "pages_http": 12, "pages_browser": 0 },
  "provider_cost_micro_usd": 0,
  "description": "Job j-1 (12 http)",
  "job_id": "j-1"
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `operation` | string | yes | Product-specific name, see 4.3 |
| `units` | object of integer ≥ 0 | yes | Unit names, see 4.3. Unknown names are stored and ignored by pricing. |
| `provider_cost_micro_usd` | integer ≥ 0 | no (default 0) | Real money the engine paid upstream (LLM tokens, OCR API). Priced by markup, see 5. |
| `description` | string | no | Human label for the ledger |
| `job_id` | string | no | |
| `credits` | integer ≥ 0 | no, **deprecated** | Used only when `units` prices to nothing and the row comes from the legacy hosted Scrapix. Removed next release. |

### 4.3 Operations and units per product

| Product | Operations | Units |
|---------|-----------|-------|
| `scrapix` | `scrape`, `map`, `search`, `parse`, `ocr`, `extract`, `crawl` | `pages_http`, `pages_browser`, `requests`, `documents`, `bytes_out` |
| `glutony` | `ingest` | `documents`, `bytes_in`, `step_seconds`, `llm_tokens_in`, `llm_tokens_out`, `audio_seconds`, `ocr_pages` |
| `lumen` | `chat`, `embed`, `rerank`, `systemone`, or `gateway` when one event aggregates every capability of a key | `requests`, `tokens_in`, `tokens_out`, `tokens_estimated` |

Engines may add units; the Lab prices only the ones in `pricing.yml` and ignores the rest. Lumen's outbox keeps one settled-cost watermark per key, so it reports `operation: "gateway"` with the key's aggregate units and `provider_cost_micro_usd` = the settled cost delta; per-capability operations are allowed when an engine tracks them separately.

### 4.4 Job events (Scrapix and glutony)

Unchanged from today's Scrapix schema: `job.completed` `{job_id, index_uid, pages_crawled, documents_indexed, duration_secs}`, `job.failed` `{job_id, error_message, pages_crawled}`. glutony maps `pipeline_uid` to `index_uid` and `documents` to `documents_indexed`, `pages_crawled` = 0. Lumen never sends job events.

## 5. Pricing (Lab)

`saas/config/pricing.yml`:

```yaml
micro_usd_per_credit: 10000        # 1 credit = $0.01
provider_cost_markup: 1.2          # pass-through cost × 1.2, then converted to credits, rounded up
products:
  scrapix:
    scrape:  { pages_http: 1, pages_browser: 5 }
    map:     { requests: 1 }
    search:  { requests: 1 }
    parse:   { documents: 1 }
    ocr:     { documents: 5 }
    extract: { documents: 2 }
    crawl:   { pages_http: 1, pages_browser: 5 }
  glutony:
    ingest:  { documents: 1, ocr_pages: 5, audio_seconds: 0 }
  lumen:
    chat:    { requests: 0 }
    embed:   { requests: 0 }
    rerank:  { requests: 0 }
    systemone: { requests: 0 }
    gateway: { requests: 0 }
```

`credits = Σ units[u] × price[product][operation][u] + ceil(provider_cost_micro_usd × markup / micro_usd_per_credit)`. The numbers above are the current Scrapix engine prices carried over; Lumen and glutony LLM work is priced through `provider_cost_micro_usd`. A missing product/operation prices to 0 and is logged once.

`saas/config/plans.yml` (plan limits, served to engines):

```yaml
free:       { concurrent_jobs: 1,   rate_limit_rpm: 60,   max_depth: 3,  js_rendering: false }
starter:    { concurrent_jobs: 3,   rate_limit_rpm: 300,  max_depth: 5,  js_rendering: true }
pro:        { concurrent_jobs: 10,  rate_limit_rpm: 1200, max_depth: 10, js_rendering: true }
enterprise: { concurrent_jobs: 100, rate_limit_rpm: 6000, max_depth: 20, js_rendering: true }
```

## 6. Internal API changes (Lab-owned `contracts/lab-internal.openapi.json`)

- Add `POST /internal/events` (it was never in the spec).
- Add `GET /internal/instances/me`.
- `/internal/auth/introspect` and `/internal/accounts/{id}` responses gain `"limits": { "concurrent_jobs", "rate_limit_rpm", "max_depth", "js_rendering" }` from `plans.yml`.
- Security scheme: `instanceSecret` (Bearer) + `X-Lab-Instance-Id` header, replacing `serviceToken`.
- All changes are additive; the legacy scheme stays documented as deprecated for one release.

## 7. Ledger and console

- `lab_events_received` gains `instance_id uuid` (the hosted engine).
- `transactions.metadata` for `usage_deduction` becomes `{ lab_event_id, product, instance_id, operation, units, provider_cost_micro_usd }`.
- `GET /instances/:id/usage?since=&until=` (new) aggregates, for a built-in row, the account's events on its hosted engine: totals per operation and per day, plus the credits debited.
- `GET /account/billing/transactions` rows carry `product` and `instance_id` so the console can filter.
- Console: the Gateway and Pipelines sections show the built-in hosted row (like Scrapix's today); "Add instance" for those kinds appears only when `NEXT_PUBLIC_LAB_BYO_ENGINES=true`. Settings for a built-in row expose name and default only (no URL, no secrets).

## 8. Enforcement and multi-tenancy (engines)

### 8.1 Pre-check

Before starting billable work for an account, an engine calls `GET /internal/accounts/{id}` (cached 30 s, stale up to 300 s, as Scrapix does today) and refuses when `credits.balance <= 0`. Past the stale window with the Lab unreachable: 503 (fail closed), as Scrapix does. Scrapix drops its tier constants and reads `limits` from the response. glutony adds the pre-check in the gateway before starting a job.

### 8.2 Lumen leases

Lumen keeps its lease model: one budget group per account on the hosted gateway (`account_ref` = Lab account id), hard budget enforced in process. The Lab provisions the group on an account's first Gateway use (stored as `instances.lumen_group_id` on the built-in row) and keeps its lease in step with the account's credits: `SyncLumenLeasesJob` (every minute, and after every Lumen debit or top-up) sets the group's budget so that `budget_max_micro - spent_micro = effective_credits_balance × micro_usd_per_credit / provider_cost_markup`, through Lumen's admin group endpoints. An account at zero credits therefore has a zero remaining lease and Lumen refuses its keys itself.

### 8.3 Lumen per-account scoping (new in Lumen)

A shared hosted Lumen serves many accounts through one master key, so Lumen must enforce the account boundary itself. When an `/admin/*` request carries `X-Lumen-Account-Ref: <uuid>`:

- list endpoints (keys, groups, usage, usage export, webhooks) return only rows whose group `account_ref` equals it;
- get/update/rotate/grant/delete on a key or group whose `account_ref` differs answer 404;
- creating a key requires a group with that `account_ref`; creating a group forces `account_ref` to the header's value;
- provider, model and config endpoints answer 403 (`LM-xxxx`, platform-only) with the header present.

Without the header the master key keeps today's unscoped behaviour (platform operator). The Lab sets the header on every proxied call for a built-in row and never exposes provider/config pages for hosted Lumen to users.

## 9. Transition

1. Lab release 1: accepts both auth schemes, ships `pricing.yml`, prices `units` when present, else uses legacy `credits`. Hosted engines minted. Built-in rows for Lumen and glutony created on first use. BYO registration gated by `LAB_BYO_ENGINES`.
2. Engines: read `LAB_INSTANCE_*`, send the new headers, send `units` (Scrapix stops sending `credits`), vendor the Lab schema, enable drift checks. Lumen adds §8.3.
3. Lab release 2: `LAB_LEGACY_*` unset in production, `credits` field dropped from the schema.

## 10. Out of scope (later)

Meilisearch Cloud connect (personal token + Platform API), per-account region choice and hosted-engine admin UI, Helm charts, Scrapix API multi-replica state, Lumen Postgres, glutony vs meili-ingest rename, removal of the flagged BYO code.
