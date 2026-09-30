# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workflow: Linear Issue Tracking

When creating a plan or proposing any major addition/feature to the project, **always create a Linear issue first** using the Linear MCP tool:
- **Team:** SCR (`https://linear.app/meilisearch/team/SCR/`)
- **Project:** "Console — Yet Another Meilisearch UI" (`https://linear.app/meilisearch/project/console-yet-another-meilisearch-ui-8f6681d804f7`)

The issue should contain the plan summary, scope of changes, and affected files. Do this before starting implementation.

## Pre-Commit Checks

Before every commit, **always** run these commands in order and fix any issues:

```bash
cargo fmt
cargo check
cargo clippy
```

All three must pass with no errors before committing.

## Project Overview

Scrapix is a high-performance, distributed web crawler and search indexer built in Rust. It's designed for internet-scale crawling with three main use cases: global internet indexing, targeted site crawling, and real-time information retrieval.

## Build Commands

```bash
# Build all crates
cargo build

# Build for production (with LTO, single codegen unit)
cargo build --release

# Build specific binary
cargo build --bin scrapix-api
cargo build --bin scrapix-worker-crawler
cargo build --bin scrapix-worker-content
cargo build --bin scrapix-frontier-service
cargo build --bin scrapix-cli

# Check without building
cargo check

# Format code
cargo fmt

# Lint
cargo clippy
```

## Testing

```bash
# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p scrapix-core
cargo test -p scrapix-parser
cargo test -p scrapix-frontier

# Run a specific integration test
cargo test --test parser_extractor
cargo test --test frontier
cargo test --test crawl_pipeline
cargo test --test incremental_crawling
cargo test --test link_graph
cargo test --test dns_cache

# Run benchmarks
cargo bench -p scrapix-benchmarks
cargo bench --bench integrated_benchmarks
cargo bench --bench wikipedia_e2e
```

## Running Locally (Recommended)

**Prerequisites:** `just`, `overmind`, `tmux`, `cargo-watch` (all via Homebrew)

```bash
# Start everything — infrastructure + all services + console
just dev

# Or step by step:
just infra        # Start Docker infra (Redpanda, Meilisearch, DragonflyDB, Postgres, ClickHouse)
just services     # Start all Rust services + console via overmind

# Manage individual services
just logs api     # Attach to API service logs (overmind connect)
just restart api  # Restart just the API service
just stop         # Stop everything (services + infra)
```

**How it works:**
- Infrastructure runs in Docker (via `docker-compose.dev.yml` overlay which disables app services)
- Rust services run natively with `cargo-watch` — shared `target/` dir means one incremental build (~3-5s)
- Console runs natively with `npm run dev`
- All managed by overmind (tmux-based process manager)
- Environment loaded from `.env` via `set dotenv-load` in the justfile
- `just dev` is the hosted (Rails) stack: `.env` must set `SCRAPIX_MODE=hosted`
  (`.env.example` does). An older `.env` without it starts the API in
  standalone mode, which refuses to run without `SCRAPIX_ADMIN_KEY`.
- The hosted engine also needs `LAB_URL`, `LAB_EVENTS_SECRET` and
  `LAB_SERVICE_TOKEN` (`.env.example` has dev values; Rails reads the same
  secret and token) and its **own database**: `.env`'s `DATABASE_URL` is the
  Rails one, so `Procfile.dev` and `just api` run the engine with
  `DATABASE_URL=$ENGINE_DATABASE_URL` (`scrapix_engine` on the same Postgres,
  created on first `just infra`; a dev volume from before the Lab split needs
  `createdb -h localhost -p 5433 -U scrapix scrapix_engine` once). An older
  `.env` without these makes `just dev` fail at engine startup.
- **Rails tests:** `cd saas && bin/rails db:prepare RAILS_ENV=test && bin/rails test`
  (needs Postgres on :5433, i.e. `just infra`). `just saas-test` runs the same
  thing but the justfile loads the repo `.env`, so it unsets `RESEND_API_KEY`
  (a real key there would make the mailer attempt SMTP from tests) and
  `LAB_CRON_ENABLED` first.

**Individual service commands** (when you only need one):
```bash
just api       # cargo watch for scrapix-api only
just frontier  # cargo watch for frontier only
just crawler   # cargo watch for crawler only
just content   # cargo watch for content only
just console   # npm run dev for console only
```

Start a crawl:
```bash
scrapix crawl -p examples/simple-crawl.json
```

## Docker Compose

Docker Compose is available for full-stack containerized development, but `just dev` (native services) is faster for iteration.

```bash
# Full stack in containers with file watching
docker compose watch

# Infrastructure only (for use with `just services`)
docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d

# Stop (preserves build caches in named volumes)
docker compose down

# Stop and remove all volumes (full reset)
docker compose down -v
```

## Diagnostic CLI Commands

Use these commands to quickly analyze system state during debugging (requires API server running):

```bash
# System-wide stats (jobs, domains tracked, success rate)
scrapix stats
scrapix stats -o json

# Recent errors with status codes and domain breakdown
scrapix errors --last 20
scrapix errors --job <job_id>
scrapix errors -o json

# Per-domain statistics (requests, success rate, avg latency)
scrapix domains --top 20
scrapix domains --filter wikipedia
scrapix domains -o json

# Check API health
scrapix health
```

**API endpoints (for programmatic access):**
- `GET /stats` - System stats (job counts, domain counters, error counts)
- `GET /errors?last=20&job_id=X` - Recent errors with distributions
- `GET /domains?top=20&filter=X` - Per-domain request stats

**Notes:**
- All diagnostic data is from in-memory tracking (recent only, since API startup)
- Error ring buffer holds last 1000 errors
- Domain counters are aggregated from crawl events

## Analytics API (Tinybird-style)

When ClickHouse is configured (`CLICKHOUSE_URL` environment variable):
1. The **Rust engine persists crawl events to ClickHouse** - PageCrawled and PageFailed events are batched (100 events) and flushed every 5 seconds
2. The **Rust engine serves the analytics API** at `/analytics/v0/pipes/` (port 8080), **scoped per account**: API-key, OAuth and session callers see only their own account (every query filters on `account_id`, and an `account_id` parameter naming another account is a 404). In standalone the admin key sees every account and `account_id` is an optional filter; the four account-level pipes (`account_usage`, `account_daily_usage`, `account_daily_usage_by_operation`, `api_key_usage`) then need `account_id`. The code is `bins/scrapix-api/src/analytics_pipes.rs`; the Rails copy (`saas/app/controllers/analytics_controller.rb`) stays until the cleanup release, and `contracts/analytics_parity.py` diffs the two.

This provides long-term analytics storage beyond the in-memory diagnostics.

### CLI Commands

```bash
# List available analytics pipes
scrapix analytics pipes

# Key performance indicators
scrapix analytics kpis --hours 24

# Top domains by request count
scrapix analytics top-domains --hours 24 --limit 10

# Stats for a specific domain
scrapix analytics domain-stats --domain example.com --hours 24

# Hourly crawl statistics
scrapix analytics hourly --hours 24

# Error breakdown by status code
scrapix analytics error-dist --hours 24

# Job statistics
scrapix analytics job-stats --job-id abc123

# JSON output
scrapix analytics kpis -o json
```

### REST API

**List available pipes:**
```bash
curl http://localhost:8080/analytics/v0/pipes
```

**Available pipes:**
```bash
# Top domains by request count
curl "http://localhost:8080/analytics/v0/pipes/top_domains.json?hours=24&limit=10"

# Stats for a specific domain
curl "http://localhost:8080/analytics/v0/pipes/domain_stats.json?domain=example.com&hours=24"

# Hourly crawl statistics
curl "http://localhost:8080/analytics/v0/pipes/hourly_stats.json?hours=24"

# Error breakdown by status code
curl "http://localhost:8080/analytics/v0/pipes/error_distribution.json?hours=24"

# Job statistics
curl "http://localhost:8080/analytics/v0/pipes/job_stats.json?job_id=abc123"

# Key performance indicators
curl "http://localhost:8080/analytics/v0/pipes/kpis.json?hours=24"
```

**Response format (Tinybird-compatible):**
```json
{
  "meta": [{"name": "domain", "type": "String"}, ...],
  "data": [...],
  "rows": 10,
  "statistics": {"elapsed": 0.015, "rows_read": 10, "bytes_read": 0}
}
```

## Architecture

### Two Backends (SCR-85 split, Lab split phase 1)

The backend is deliberately split into two services that **share no
database**: the engine never touches the Lab's Postgres (enforced by
`bins/scrapix-api/tests/lab_boundary.rs`, which fails the build on any SQL that
names a Lab-owned table, reads included), and talks to the Rails app (the
"Lab") only over HTTP:

- **Rails SaaS control plane (`saas/`, port 8081)** — auth + sessions, social
  login, account/team/invites, API keys, billing + Stripe, saved crawl
  configs + engines CRUD, the OAuth 2.1 provider, and the MCP server at
  `/mcp`. It also serves the engine's `/internal/*` API (contract:
  `contracts/lab-internal.openapi.json`). `meilisearch_engines.api_key` is
  encrypted at rest (Active Record encryption, `ACTIVE_RECORD_ENCRYPTION_*`).
  Contract-tested by `contracts/`.
- **Rust crawl engine (`bins/scrapix-api`, port 8080)** — /scrape, /map,
  /search, /crawl*, jobs, WebSockets, diagnostics, and the analytics pipes
  (`/analytics/v0/pipes/*`, scoped per account). It keeps its own store
  (`DATABASE_URL`: jobs, job results, the lab-events outbox; SQLite by default
  or its own Postgres, migrated by the engine itself in **both** modes) and
  resolves everything it needs from the Lab through `LabClient`
  (`{LAB_URL}/internal/*`, Bearer `LAB_SERVICE_TOKEN`): credentials are sent to
  `POST /internal/auth/introspect` (cached for the Lab's `cache_ttl`, default
  30 s, stale up to 5 min if the Lab is down; an unknown credential while the
  Lab is down is a 503 with `Retry-After: 5`), the Meilisearch target comes
  from `GET /internal/accounts/{id}/meilisearch`, and credits from
  `GET /internal/accounts/{id}`. It issues no credentials and reads no Lab
  table. At startup it pings `{LAB_URL}/internal/ping` and refuses to start on
  a wrong token or a 4xx (wrong `LAB_URL`); the metric
  `scrapix_lab_requests_total{endpoint,outcome}` counts these calls. It also
  **reports lab events**: usage (`usage.recorded`) and job lifecycle (`job.completed` /
  `job.failed`) go to an engine-owned `lab_events` outbox and are delivered,
  HMAC-signed, to Rails' `POST /internal/events` (contract:
  `contracts/lab-events.schema.json`, docs `docs/api-reference/lab-events.mdx`).
  Rails owns everything that follows from them: idempotent credit debits,
  auto top-up (Stripe, saved card only), low-balance and job emails, the
  saved-config cron (`RunDueCrawlConfigsJob`, gated by `LAB_CRON_ENABLED`,
  which calls the engine with `LAB_SERVICE_TOKEN`), and OAuth token cleanup.
  The hosted engine refuses to start without `LAB_URL`,
  `LAB_EVENTS_SECRET` and `LAB_SERVICE_TOKEN`, and uses its own
  `DATABASE_URL` (SQLite if unset; use its own Postgres in production); the
  engine never reads `STRIPE_SECRET_KEY` or `JWT_SECRET`.

The console proxy (`console/src/app/api/scrapix/[...path]/route.ts`) routes
by path prefix via `SAAS_API_URL` + `SAAS_PREFIXES`; the frozen full-platform
spec is `contracts/openapi.json`, the engine-only spec is
`contracts/openapi.engine.json`.

#### Standalone vs hosted

The engine has the same store in both modes: a job-history store it migrates
itself (SQLite by default, or a dedicated Postgres), and it refuses to start
against a database that holds the Rails schema. `bins/scrapix-api` runs
**standalone** (`SCRAPIX_MODE=standalone`, the default) with no Rails control
plane at all: one operator key (`SCRAPIX_ADMIN_KEY`) instead of
accounts/sessions/API keys. `SCRAPIX_MODE=hosted` adds the Lab: it requires
`LAB_URL`, `LAB_EVENTS_SECRET` and `LAB_SERVICE_TOKEN` (plus the engine's own
`DATABASE_URL` in production), fails closed if one of the three is missing, and ignores `JWT_SECRET`
(the Lab verifies sessions). Docs for self-hosters live in `docs/` (this is the
product repo's docs site); platform-only docs (accounts, billing, API key
CRUD, OAuth) live in `saas/docs/` and will eventually merge into the
Meilisearch Lab docs. See `docs/deployment/self-hosting.mdx`.

### Workspace Structure

The project is organized as a Cargo workspace with two main directories:

**Library Crates (`crates/`):**
- `scrapix-core` - Shared types, traits, configuration schemas, error types
- `scrapix-frontier` - URL frontier with bloom filter deduplication, priority scheduling, SimHash/MinHash near-duplicate detection
- `scrapix-crawler` - HTTP fetching (reqwest), JS rendering (chromiumoxide), robots.txt, DNS caching, proxy rotation
- `scrapix-parser` - HTML parsing (scraper), content extraction, markdown conversion, language detection
- `scrapix-extractor` - Feature extraction: metadata, JSON-LD/microdata schemas, custom CSS selectors, block splitting
- `scrapix-ai` - AI enrichment via OpenAI: extraction, summarization, embeddings
- `scrapix-storage` - Storage backends: Meilisearch, RocksDB, Redis/DragonflyDB, S3/MinIO, ClickHouse
- `scrapix-queue` - Kafka/Redpanda message queue producer and consumer
- `scrapix-telemetry` - Prometheus metrics, distributed tracing, structured logging

**Binary Crates (`bins/`):**
- `scrapix-api` - REST API server (axum) with WebSocket support
- `scrapix-worker-crawler` - Crawler worker that fetches URLs from the frontier
- `scrapix-worker-content` - Content processor that parses HTML and indexes to Meilisearch
- `scrapix-frontier-service` - Frontier service managing URL queue and deduplication
- `scrapix-cli` - CLI tool for starting crawls and checking status

**Frontend (`console/`):**
- Next.js 16 app (App Router) with TypeScript, Tailwind CSS v4, shadcn/ui
- Runs on port 3001 (`npm run dev`)
- `Dockerfile.dev` for containerized development with `docker compose watch`

### Data Flow

1. API receives crawl request → publishes to Redpanda
2. Frontier Service deduplicates URLs → assigns priorities → partitions by domain
3. Crawler Workers consume URLs → fetch pages → extract links → publish raw HTML
4. Content Workers consume HTML → parse → extract features → index to Meilisearch

### Key Technologies

- **Message Queue:** Redpanda (Kafka-compatible, via rdkafka crate)
- **Search:** Meilisearch (primary store for documents, metadata, vectors)
- **Local State:** RocksDB (per-worker URL cache, robots.txt, DNS)
- **Cache:** DragonflyDB/Redis (rate limiting, real-time counters)
- **Object Storage:** S3-compatible (RustFS/MinIO) for HTML archives

### Near-Duplicate Detection

The frontier uses dual locality-sensitive hashing:
- **SimHash:** 64-bit fingerprints for quick similarity checks (Hamming distance threshold ~10 bits)
- **MinHash:** 128 hash functions for accurate Jaccard similarity estimation (threshold ~0.8)

## Crawl Pipeline Behavior

### Job lifecycle: completion, cancel, pause/resume

A job completes from **exact accounting** of the work derived from it (dispatched
URLs, crawled/failed pages, indexed documents) — not an idle timer. Once every
dispatched URL is accounted for, the job waits `JOB_COMPLETION_GRACE_MS`
(default `3000`, `bins/scrapix-api/src/lib.rs`) for straggler events before
finalizing, to absorb ordinary event-arrival jitter. A job with **zero crawled
pages** is marked `Failed` rather than `Completed`. A job that stops making
progress for `JOB_STALL_TIMEOUT_SECS` (default `1800`) is finalized as
`FailStalled`; **stalled jobs are billed** for the pages they did crawl.
`Replace`-strategy stale-document cleanup only runs after true completion, and
never deletes a document whose page returned `304` in this job.

- `DELETE /job/{id}` cancels a pending, running or paused job: it stops the
  frontier and every worker for that job and bills the pages crawled so far.
  Only a non-terminal job can be cancelled — cancelling a `completed`,
  `failed` or `cancelled` job returns `409`.
- `POST /job/{id}/pause` / `POST /job/{id}/resume` — the frontier stops
  dispatching a paused job's URLs (in-flight pages finish, and links they
  discover keep being **admitted** into the queue, just not dispatched); a
  paused job is never auto-completed or stall-failed. Only `Running → Paused`
  and `Paused → Running` are valid; any other starting status returns `409`.
  Resuming restarts the job's stall clock from zero.
- Exactly one completion email is sent per job.
- The crawl-creation response and `GET /job/{id}/status` both carry a
  `warnings` array: non-fatal notices about config fields the engine accepted
  but could not fully honor (see "Per-job config fields" below) plus
  worker-raised warnings during the run.

### Job accounting schema

The engine migrates its own schema in both modes (`bins/scrapix-api/migrations`),
so the `jobs.accounting` column is always present and there is no degraded,
in-memory-only accounting mode.

### Durability, controls and rollout caveats

- **Upgrade with no job running** (drain or cancel first): a job spanning
  the upgrade loses the old in-memory frontier queue and has no restored
  accounting, so it ends `FailStalled` (billed). Checklist:
  `docs/operations/crawl-engine-rollout.mdx`.
- `INSTANCE_ID` (frontier) / `WORKER_ID` (crawler, content) must be **stable
  per instance**: they name the job-control consumer group; a random id
  (default, warned at startup) misses controls sent while down and leaks a
  group per restart.
- On one box, give each service a distinct `WAKE_PORT` (default `8081`
  collides with the Rails SaaS; a failed bind only warns and metrics go
  missing), e.g. frontier 9101, crawler 9102, content 9103.
- The `REDIS_URL` Redis needs persistence (AOF or RDB) for frontier
  durability.
- Graceful frontier shutdown requeues popped-but-unsent URLs
  (`DISPATCH_SHUTDOWN_GRACE_MS`, 10 s); store calls are bounded at 5 s. A
  **crash** between pop and send still loses that batch.
- Accounting seen-sets are not persisted: a duplicate outcome whose original
  landed before an API restart can be double-counted (rare).
- The API publishes `JobControl`s in request order (one queue, one task);
  a Running job silent and unbalanced for `RESUME_HEAL_AFTER_SECS` (60) gets
  `Resume` re-published (no-op at the frontier for running jobs).
- Stored/returned job configs mask proxy credentials and custom header
  values (as well as the Meilisearch key and webhook secrets); the crawler
  uses the in-memory values.

### Politeness

Defaults: `CONCURRENT_PER_DOMAIN=4`, `DOMAIN_DELAY_MS=250`
(`bins/scrapix-frontier-service/src/lib.rs`), down from 50 / 50: single-site
crawls are roughly 5–12× slower unless the frontier env overrides them
(e.g. `DOMAIN_DELAY_MS=50 CONCURRENT_PER_DOMAIN=16`; a job can only raise
the delay, never lower it). The frontier holds a per-domain
slot from dispatch until the crawler's `FetchFeedback` — topic
`scrapix.fetch.feedback` — reports the fetch back (not at dispatch time); a
slot without feedback expires after `2 × REQUEST_TIMEOUT` (default `30`s), so
a lagging `scrapix.urls.processing` consumer effectively widens that safety
window. The effective per-domain delay is
`max(DOMAIN_DELAY_MS, job.rate_limit.per_domain_delay_ms, robots Crawl-delay
if respected, 1000 / requests_per_second)`. `rate_limit.default_crawl_delay_ms`
(config default `0`) is a fallback `Crawl-delay` used only when robots.txt was
fetched and set no `Crawl-delay` of its own, and the job set no explicit delay
or rate — it never lowers an explicit value. With `REDIS_URL` set on the
frontier, politeness state (and the frontier's admission/dedup state, via
`FrontierStore`) is shared across every frontier instance, so more than one
frontier instance can run at once (each holds a per-job dispatch lease).

### `/metrics`

The engine (`scrapix-api`) exposes unauthenticated Prometheus text-format
metrics at `GET /metrics` on its normal HTTP port (8080). Every worker
(`scrapix-worker-crawler`, `scrapix-worker-content`,
`scrapix-frontier-service`) exposes the same `/metrics` and a `GET /health`
on its `WAKE_PORT` (default `8081`) — the same bare-TCP listener Fly's proxy
uses to autostart a suspended machine. Metric names are a contract with
`qdq-server/monitoring` (see `crates/scrapix-core/src/metrics.rs`): don't
rename without updating the scrape config there. Current metrics:
`scrapix_crawler_fetches_total{outcome}`,
`scrapix_crawler_fetch_duration_seconds`, `scrapix_crawler_bytes_total`,
`scrapix_frontier_admissions_total{result}`, `scrapix_frontier_queued{job}`,
`scrapix_frontier_dispatched_total`, `scrapix_content_documents_total{outcome}`,
`scrapix_content_flush_duration_seconds`, `scrapix_api_jobs{status}`,
`scrapix_consumer_uncommitted{topic}`, `scrapix_lab_events_pending`,
`scrapix_lab_events_delivered_total{outcome}` (`accepted`|`rejected`|`failed`),
`scrapix_lab_requests_total{endpoint,outcome}` (the engine's calls to the Lab's
`/internal/*` API).

### Webhooks (SCR-72)

Body: `{"event": "<snake_case>", "job_id", "timestamp", "data": <event JSON>}`,
headers `X-Scrapix-Event` (same event name) and `X-Scrapix-Delivery` (one UUID
per delivery, stable across its retries — dedupe on this, order by
`timestamp`, since concurrent delivery means events can arrive out of order).
Auth: `Bearer { token }`, `Headers { headers }` (validated against
`Content-Type`/`Host`/`X-Scrapix-*` collisions), or
`Hmac { secret, algorithm: "sha256", header }` sending
`header: sha256=<hex hmac-sha256 of the body>`. `timeout_ms` is clamped to
1000–30000ms. Up to 3 attempts total (1s then 5s backoff); a 4xx response is
terminal, a network error or 5xx retries. `crawl_failed` is also how
cancellation is reported (`data.error == "cancelled"`); `batch_sent` is
accepted in a hook's `events` list but never fires — there is no "batch
flushed to the store" `CrawlEvent` today. **Known limitation:** webhook
configs live in the API process's memory only (never persisted), so a job
that spans an API restart stops delivering webhooks after the restart.

### Per-job config fields now honored

Per job: `headers`, `user_agents` (rotation), `proxy` (urls/rotation/tiered,
`http`/`https` only — `socks5`/`socks5h` are rejected at job creation),
`crawler_type: browser` (JS rendering), `rate_limit.*`, `concurrency.max_concurrent_requests`,
`sitemap.enabled` (**default: `true`**, was `false`) / `sitemap.urls`,
`url_patterns.index_only`, every feature's `include_pages`/`exclude_pages`,
`schema.only_types`/`convert_dates`, `meilisearch.primary_key`/`batch_size`/
`settings`/`keep_settings`, `webhooks`, `features.pdf.*` (incl. `max_pages`,
`extract_links`), `features.documents`, `features.ocr`. A browser job (`crawler_type:
"browser"`) that also sets `proxy` fails closed — its warning says the shared
browser only has one worker-level proxy, so browser-rendered pages of that
job fail rather than silently connect unproxied.

Fields accepted but **worker-level only** (ignored per job, warned about when
set to a non-default value): `concurrency.browser_pool_size`,
`concurrency.dns_concurrency`.

### Documents & OCR (SCR-81, SCR-86)

Binary documents go through one format dispatch in `scrapix-parser`
(`document.rs`): Content-Type first, magic bytes as fallback (never the URL
extension). PDFs → `pdf-inspector` (classification + per-page OCR
recommendation + layout-aware Markdown with tables); Word/PowerPoint/Excel/
OpenDocument/RTF/EPUB/CSV → `anydoc`. Both pinned exactly (fast-moving).
The crawl path (fetcher base64 transport, gated by `features.pdf` /
`features.documents`, predicate `scrapix_core::content_types::is_binary_document`
on both sides of Kafka), `POST /scrape` of a document URL and
`POST /parse` (multipart upload, `bins/scrapix-api/src/documents.rs`) all
share it. Scanned pages are flagged (`metadata.needs_ocr`), never silently
blank. OCR (`scrapix-ocr`) is opt-in (`off`/`auto`/`force`): PDFium
rasterization (runtime-loaded, `PDFIUM_LIB_PATH`), vision-LLM (via
`scrapix-ai`, usage tracked as feature `ocr`) or Tesseract backend, page cap,
per-account daily budget, page-image-hash cache (hits not billed). OCR pages
cost `OCR_PAGE_CREDITS` (5) each — separate `ocr` ledger entry on
scrape/parse, `pages_ocr` in crawl accounting (`DocumentIndexed.ocr_pages`),
`request_events.ocr_pages` in ClickHouse. Fixtures + generator:
`crates/scrapix-parser/tests/fixtures/`. Guide: `docs/guides/documents.mdx`.

### `scrapix all` limitation

The in-process channel bus used by `scrapix all` has no redelivery on
failure — at-least-once delivery only holds when running over Kafka/Redpanda.

## Marketing Product Pages

Product landing pages live in `console/src/app/(marketing)/products/{scrape,map,crawl,search}/page.tsx`. All pages follow a consistent section structure and visual pattern.

### Section Order

1. **Hero** — Badge with icon + endpoint name, h1 with glitch-font highlighted word, subtitle, CTA buttons (Try it free / See pricing)
2. **Code example** — Faux-terminal with three dot header, curl command, and inline JSON response preview
3. **How it works** — Numbered steps (`"01"`, `"02"`, etc.) using Rubik Glitch font (`var(--font-rubik-glitch)`) at `text-3xl` with gradient text
4. **Features grid** — 6 cards in a 3-col grid, each with a 10x10 icon container (`rounded-xl bg-gradient-to-br ... ring-1 ring-white/10`)
5. **Checklist** — Two-column grid of features with green checkmark icons
6. **Pricing summary** — Simple label/value rows in a bordered card, link to full pricing
7. **CTA** — Centered heading + subtitle + single button, background glow

### Color Themes Per Product

| Product | Primary | Gradient | Glow |
|---------|---------|----------|------|
| Scrape | `indigo-400` | `from-indigo-400 to-cyan-400` | `bg-indigo-500/10` |
| Map | `cyan-400` | `from-cyan-400 to-indigo-400` | `bg-cyan-500/10` |
| Crawl | `violet-400` | `from-violet-400 to-indigo-400` | `bg-violet-500/10` |
| Search | `emerald-400` | `from-emerald-400 to-cyan-400` | `bg-emerald-500/10` |

### Key Conventions

- All API URLs in examples use `https://scrapix.meilisearch.dev`
- Hero highlighted word uses `style={{ fontFamily: "var(--font-rubik-glitch), var(--font-geist-sans), sans-serif" }}`
- Step numbers use the same glitch font with gradient `bg-clip-text text-transparent`
- Terminal response uses color classes: `text-indigo-400` for keys, `text-emerald-400` for strings, `text-cyan-400` for numbers, `text-zinc-600` for punctuation
- Navigation links exist in both the header dropdown and footer in `console/src/app/(marketing)/layout.tsx`

## Billing Data Model

The system tracks usage data for pricing/billing purposes.

### Data Tracked Per Request

| Field | Type | Description |
|-------|------|-------------|
| `account_id` | String | Account for billing attribution |
| `content_length` | u64 | Bytes downloaded (bandwidth billing) |
| `js_rendered` | bool | Premium JS rendering feature |
| `job_id` | String | Job attribution |
| `domain` | String | Domain crawled |

### Billing Types (scrapix-core)

- `Account` - Billable entity with tier and quotas
- `ApiKey` - Authentication token linked to account
- `BillingTier` - Free/Starter/Pro/Enterprise with limits
- `UsageMetrics` - Per-period usage tracking

### ClickHouse Analytics Queries

```sql
-- Account usage summary
SELECT account_id, count() as pages, sum(content_length) as bytes
FROM crawl_events
WHERE crawled_at >= now() - INTERVAL 30 DAY
GROUP BY account_id;

-- Daily breakdown for billing
SELECT toDate(crawled_at) as date, count() as requests, sum(content_length) as bytes
FROM crawl_events
WHERE account_id = 'acct_123'
GROUP BY date ORDER BY date;
```

### API Endpoints for Billing

- `GET /analytics/v0/pipes/account_usage.json?account_id=X&hours=24` - Account usage
- `GET /analytics/v0/pipes/top_accounts.json?hours=24&limit=10` - Top accounts

## Environment Variables

| Variable | Description |
|----------|-------------|
| `SCRAPIX_MODE` | API: `standalone` (default) or `hosted`. Validated at startup; an unrecognized value refuses to start |
| `SCRAPIX_ADMIN_KEY` | API, standalone only: the operator key guarding every protected route (`Authorization: Bearer` or `X-API-Key`; `?token=` on WebSocket routes). Required unless `SCRAPIX_AUTH=disabled`; must be ≥16 chars after trimming |
| `SCRAPIX_AUTH` | API, standalone only: `disabled` turns off all authentication (local dev only, logs a loud warning; refused in hosted mode and together with `SCRAPIX_ADMIN_KEY`) |
| `DATABASE_URL` | API: the engine's **own** job-history store in both modes; default `sqlite://./data/scrapix.db` (image default `sqlite:///data/scrapix.db`), or a dedicated `postgres://`/`postgresql://` URL the engine migrates itself (refuses a database that already has the Rails schema, so it can never be the Lab's). Hosted production should point it at a dedicated Postgres database (e.g. `scrapix_engine`). (Rails has its own `DATABASE_URL`, the Lab database.) |
| `JWT_SECRET` | Rails only: signs session JWTs. The engine ignores it (logs a line if set) — the Lab verifies sessions |
| `ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY` / `ACTIVE_RECORD_ENCRYPTION_DETERMINISTIC_KEY` / `ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT` | Rails only, required: encrypt `meilisearch_engines.api_key` at rest. Generate with `cd saas && bin/rails db:encryption:init`; `.env.example` has dev placeholders |
| `KAFKA_BROKERS` | Kafka/Redpanda broker addresses |
| `MEILISEARCH_URL` | Meilisearch server URL |
| `MEILISEARCH_API_KEY` | Meilisearch API key |
| `REDIS_URL` | Redis/DragonflyDB URL |
| `CLICKHOUSE_URL` | ClickHouse HTTP URL (enables analytics API) |
| `CLICKHOUSE_DATABASE` | ClickHouse database name (default: scrapix) |
| `CLICKHOUSE_USER` | ClickHouse username |
| `CLICKHOUSE_PASSWORD` | ClickHouse password |
| `RUST_LOG` | Log level (info, debug, trace) |
| `OPENAI_API_KEY` | For AI enrichment features |
| `JOB_STALL_TIMEOUT_SECS` | API: seconds without progress before a job is finalized `FailStalled` (default `1800`) |
| `JOB_COMPLETION_GRACE_MS` | API: grace period after exact accounting says a job is done, before finalizing (default `3000`) |
| `MAX_PENDING_ACKS` | API: max event acks held awaiting the accounting flush before the consumer blocks (default `50000`) |
| `ALLOW_PRIVATE_IPS` | API: allow webhook deliveries to private/loopback/link-local addresses (default `false`, SSRF opt-out, tests only); same-named flag also exists on the crawler worker for its own fetches |
| `WEBHOOK_MAX_CONCURRENT_DELIVERIES` | API: max webhook deliveries in flight at once across all jobs/hooks (default `64`) |
| `LAB_URL` | API, hosted only, required: the Lab's base URL (`http://`/`https://`, no path, e.g. `http://127.0.0.1:8091`). Events go to `{LAB_URL}/internal/events`, everything else to `{LAB_URL}/internal/*`. Ignored in standalone |
| `LAB_EVENTS_URL` | **Deprecated** fallback for `LAB_URL` (the old `…/internal/events` URL; the base is derived from it, with a warning). Do not set it |
| `LAB_EVENTS_SECRET` | API + Rails, hosted only, required (≥32 chars, same value on both): HMAC-SHA256 key signing lab-event deliveries (`X-Scrapix-Signature`). Generate with `openssl rand -hex 32` |
| `LAB_SERVICE_TOKEN` | API + Rails, hosted only, required (≥32 chars, same value on both): Bearer token in both directions: Rails presents it (with `X-Scrapix-Account-Id`) when it calls the engine for an account (saved-config cron), and the engine presents it on `{LAB_URL}/internal/*` |
| `LAB_CRON_ENABLED` | Rails: `true` runs the saved-config cron (default off, so a Rails deploy can't double-fire crawls; the engine runs no scheduler of its own) |
| `DOMAIN_DELAY_MS` | Frontier: minimum per-domain delay (default `250`) |
| `CONCURRENT_PER_DOMAIN` | Frontier: max concurrent in-flight requests per domain (default `4`) |
| `FRONTIER_KEY_PREFIX` | Frontier: Redis key prefix for the frontier store (default `scrapix:frontier`) |
| `JOB_RETENTION_HOURS` | Frontier: how long a finished/cancelled job's state stays queryable after release (default `168`) |
| `WAKE_PORT` | Every worker: bare-TCP port serving `/metrics` and `/health` and triggering Fly autostart (default `8081`) |
| `DOCUMENT_MAX_SIZE_MB` | API: max document size for `/scrape` of a document and `/parse` uploads (default `50`) |
| `OCR_BACKEND` | API + content workers: `auto` (vision if an AI provider is set, else tesseract), `vision`, `tesseract`, `off` |
| `PDFIUM_LIB_PATH` | API + content workers: PDFium library used to rasterize pages for OCR (`/opt/pdfium/lib` in the images) |
| `OCR_MAX_PAGES_PER_DOCUMENT` / `OCR_DAILY_PAGE_BUDGET` | OCR cost controls (defaults `50` / `1000` per account per UTC day, `0` = unlimited) |

`BLOOM_CAPACITY`/`BLOOM_FP_RATE` on the frontier are now deprecated and
ignored — dedup lives in the `FrontierStore` (Redis or in-memory), not a
bloom filter.

## Kubernetes Deployment

```bash
# Local development (Docker Desktop)
scrapix k8s deploy
scrapix k8s port-forward

# Production
scrapix k8s deploy -o prod

# Or manually with kubectl:
kubectl apply -k deploy/kubernetes/overlays/local
kubectl port-forward -n scrapix svc/scrapix-api 8080:8080
```

## CLI Usage Guide (for Testing, Benchmarking, and Review)

This section is a guide for using the Scrapix CLI to test, benchmark, and review crawling operations.

### Quick Reference

| Task | Command |
|------|---------|
| Start infrastructure | `scrapix infra up` |
| Stop infrastructure | `scrapix infra down` |
| Run distributed crawl | `scrapix crawl -p config.json` |
| Run standalone crawl | `scrapix local -p config.json` |
| Check system status | `scrapix stats` |
| View errors | `scrapix errors --last 20` |
| View domain stats | `scrapix domains --top 10` |
| Run benchmarks | `scrapix bench all` |
| Deploy to Kubernetes | `scrapix k8s deploy` |
| Show K8s status | `scrapix k8s status` |

### Infrastructure Commands

```bash
# Start infrastructure (Redpanda, Meilisearch, DragonflyDB)
scrapix infra up

# Stop infrastructure
scrapix infra down

# Restart infrastructure
scrapix infra restart

# Show status
scrapix infra status

# View logs (optionally for specific service)
scrapix infra logs
scrapix infra logs redpanda -f

# Full reset (removes all data volumes)
scrapix infra reset
scrapix infra reset -y  # Skip confirmation
```

### Test Workflows

#### 1. Quick Single-Page Test (No Infrastructure)

For testing parser/extractor changes without starting Kafka/Meilisearch:

```bash
# Standalone crawl - fetches, parses, outputs result directly
scrapix local -c '{"start_urls":["https://example.com"],"index_uid":"test"}'
scrapix local -p config.json --output results.json
```

This bypasses the distributed system entirely. Useful for:
- Testing HTML parsing changes
- Debugging content extraction
- Quick validation without infrastructure overhead

#### 2. Full Distributed Test

For testing the complete pipeline (API → Kafka → Workers → Meilisearch):

```bash
# 1. Start infrastructure
scrapix infra up

# 2. Start all services (in separate terminals, or use screen/tmux)
KAFKA_BROKERS=localhost:19092 MEILISEARCH_URL=http://localhost:7700 MEILISEARCH_API_KEY=masterKey cargo run --release --bin scrapix-api &
KAFKA_BROKERS=localhost:19092 cargo run --release --bin scrapix-frontier-service &
KAFKA_BROKERS=localhost:19092 cargo run --release --bin scrapix-worker-crawler &
KAFKA_BROKERS=localhost:19092 MEILISEARCH_URL=http://localhost:7700 MEILISEARCH_API_KEY=masterKey cargo run --release --bin scrapix-worker-content &

# 3. Submit a crawl job
scrapix crawl -p examples/simple-crawl.json

# 4. Monitor progress
scrapix status <job_id>
scrapix stats
scrapix errors --last 10
scrapix domains --top 5
```

#### 3. Crawl Configuration Examples

**Simple single-site crawl:**
```json
{
  "start_urls": ["https://docs.example.com"],
  "max_depth": 3,
  "max_pages": 100,
  "index_uid": "test-crawl"
}
```

**Multi-site crawl with domain restrictions:**
```json
{
  "start_urls": ["https://site1.com", "https://site2.com"],
  "max_depth": 2,
  "max_pages": 500,
  "allowed_domains": ["site1.com", "site2.com"],
  "index_uid": "multi-site-test"
}
```

### Reviewing Crawl Results

#### Check Indexed Documents in Meilisearch

```bash
# Search indexed documents
curl "http://localhost:7700/indexes/test-crawl/search" \
  -H "Authorization: Bearer masterKey" \
  -H "Content-Type: application/json" \
  -d '{"q": "search term", "limit": 10}'

# Get document count
curl "http://localhost:7700/indexes/test-crawl/stats" \
  -H "Authorization: Bearer masterKey"

# Get specific document by ID
curl "http://localhost:7700/indexes/test-crawl/documents/doc_id" \
  -H "Authorization: Bearer masterKey"
```

#### Check Analytics (if ClickHouse enabled)

```bash
# Key metrics
scrapix analytics kpis --hours 24

# Domain performance
scrapix analytics top-domains --limit 10

# Error analysis
scrapix analytics error-dist --hours 24

# Job-specific stats
scrapix analytics job-stats --job-id <job_id>
```

### Benchmarking

```bash
# Run all benchmarks
scrapix bench all

# Run Wikipedia E2E benchmark
scrapix bench wikipedia

# Run integrated component benchmarks
scrapix bench integrated

# Run parser benchmarks
scrapix bench parser

# Run with multiple iterations and verbose output
scrapix bench all -i 3 -v

# Save results to custom directory
scrapix bench wikipedia -o ./my-bench-results
```

**Key benchmark targets:**
- `all` - Both wikipedia_e2e and integrated_benchmarks
- `wikipedia` - Real-world Wikipedia crawling
- `integrated` - Full pipeline performance
- `parser` - Parser/extractor microbenchmarks

### Kubernetes Commands

```bash
# Deploy to Kubernetes (local overlay)
scrapix k8s deploy

# Deploy to production
scrapix k8s deploy -o prod

# Show deployment status
scrapix k8s status
scrapix k8s status -w  # Watch mode

# View logs
scrapix k8s logs           # All components
scrapix k8s logs crawler   # Specific component
scrapix k8s logs -f        # Follow logs

# Scale components
scrapix k8s scale crawler -r 5

# Restart components
scrapix k8s restart        # All
scrapix k8s restart api    # Specific

# Port forward for local access
scrapix k8s port-forward

# Destroy deployment
scrapix k8s destroy
scrapix k8s destroy -y  # Skip confirmation
```

### Troubleshooting

| Issue | Check |
|-------|-------|
| Crawl not progressing | `scrapix stats` - check queue sizes, error counts |
| High error rate | `scrapix errors --last 50` - identify patterns |
| Slow domain | `scrapix domains --filter domain.com` - check avg latency |
| Service not connecting | Check env vars (KAFKA_BROKERS, MEILISEARCH_URL) |
| Kafka issues | `scrapix infra logs redpanda` |

### Clean Up

```bash
# Stop infrastructure
scrapix infra down

# Full reset (removes all data volumes)
scrapix infra reset

# Clean local data directories
rm -rf ./data ./bench-results ./crawl-results
```
