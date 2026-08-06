# Scrapix API Contracts

Backend-agnostic contract tests for the SaaS API surface, plus the frozen
OpenAPI specs. This was the safety net for the Rails migration
([SCR-85](https://linear.app/meilisearch/issue/SCR-85)) and remains the
regression suite for the Rails SaaS app.

Since phase 9 the split is permanent: the Rails app (`saas/`, port 8081)
serves the whole SaaS surface (auth, account/team, API keys, billing/Stripe,
configs, engines, analytics pipes, OAuth provider, `/mcp`), and the Rust API
(port 8080) is a pure crawl engine (scrape/map/search/crawl, jobs, WebSockets,
diagnostics). The engine no longer serves any SaaS route.

## Files

- `openapi.json` — the **frozen full-platform public spec** (engine + SaaS
  routes). It is the contract the Rails app implements and the source the
  Rails MCP server generates its tools from. Hand-frozen — no generator
  regenerates it anymore; edit only on an intentional, reviewed contract
  change.
- `openapi.engine.json` — the engine-only spec served by the Rust API at
  `/openapi.json`, pinned by `cargo test -p scrapix-api --test
  openapi_snapshot`. Regenerate after an intentional engine API change with
  `UPDATE_OPENAPI_SNAPSHOT=1 cargo test -p scrapix-api --test
  openapi_snapshot`, and review the diff.
- `tests/*.contract.test.ts` — live-backend contract tests (vitest).
- `src/shapes.ts` — the frozen response shapes (snake_case, exact keys —
  extra keys are failures).
- `analytics_parity.py` — live diff of the analytics pipes between two
  backends (was used to prove byte parity during the migration).

## Running

The suite targets the Rails app, with the Rust engine up for the routes that
proxy into it (config trigger, MCP product tools). Start `just infra`, the
engine, and the Rails server (with `AUTH_RATE_LIMIT=1000` — Rack::Attack's
5/min signup throttle trips the suite otherwise):

```bash
cd contracts
npm install
CONTRACT_BASE_URL=http://localhost:8081 npm test
```

Each test file signs up fresh throwaway users (`contract-*@example.com`), so
runs are self-contained; no seeding or cleanup required. Analytics tests
self-skip when ClickHouse isn't configured.

## Rules

- Shapes in `src/shapes.ts` mirror the original Rust handlers and
  `console/src/lib/api-types.ts`. Change them only when the API contract
  changes intentionally — never to make a backend pass.
