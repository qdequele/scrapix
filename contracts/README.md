# Scrapix API Contracts

Backend-agnostic contract tests for the SaaS API surface, plus the frozen
OpenAPI snapshot. This is the safety net for the Rails migration
([SCR-85](https://linear.app/meilisearch/issue/SCR-85)): both backends must
pass the same suite, so a route can only flip from Rust to Rails once Rails
reproduces the contract exactly.

## Files

- `openapi.json` — snapshot of the Rust API's OpenAPI spec, pinned by
  `cargo test -p scrapix-api --test openapi_snapshot`. Regenerate after an
  intentional API change with `UPDATE_OPENAPI_SNAPSHOT=1 cargo test -p
  scrapix-api --test openapi_snapshot`, and review the diff.
- `tests/*.contract.test.ts` — live-backend contract tests (vitest).
- `src/shapes.ts` — the frozen response shapes (snake_case, exact keys —
  extra keys are failures).

## Running

The suite needs a running backend with Postgres-backed auth enabled
(`just infra` + the API service):

```bash
cd contracts
npm install
npm test                                          # against Rust (localhost:8080)
CONTRACT_BASE_URL=http://localhost:8081 npm test  # against Rails
```

Each test file signs up fresh throwaway users (`contract-*@example.com`), so
runs are self-contained; no seeding or cleanup required. Analytics tests
self-skip when ClickHouse isn't configured.

## Rules

- Shapes in `src/shapes.ts` mirror the Rust handlers and
  `console/src/lib/api-types.ts`. Change them only when the API contract
  changes intentionally — never to make a backend pass.
- When a route migrates to Rails, its tests must pass against **both**
  backends before the proxy prefix flips.
