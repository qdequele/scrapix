# Scrapix API Contracts

The contracts the Scrapix engine owns, plus the one it vendors from the
Meilisearch Lab. The Lab (Rails control plane + console) lives in
[`meilisearch/lab`](https://github.com/meilisearch/lab), which vendors byte
copies of the engine-owned files below and drift-checks them; the Lab's
contract test suite lives there too.

## Engine-owned files

Change these only on an intentional, reviewed contract change — the Lab's
drift check fails until it re-vendors them.

- `openapi.json` — the **frozen full-platform public spec** (engine + Lab
  routes). It is the contract the Lab implements and the source its MCP
  server generates tools from. Hand-frozen — no generator regenerates it.
  The Python and TypeScript SDKs (`sdks/`) are generated from it: after
  editing it, run `just sdk-generate` and commit the result (CI's
  `just sdk-check` fails on stale SDK code).
- `openapi.engine.json` — the engine-only spec served by the Rust API at
  `/openapi.json`, pinned by `cargo test -p scrapix-api --test
  openapi_snapshot`. Regenerate after an intentional engine API change with
  `UPDATE_OPENAPI_SNAPSHOT=1 cargo test -p scrapix-api --test
  openapi_snapshot`, and review the diff.
- `lab-events.schema.json` — JSON Schema for the events the engine reports to
  the Lab (`POST {LAB_URL}/internal/events`). The engine's `lab_events.rs`
  tests and the Lab's receiver tests both validate against it; change it only
  together with both sides.

## Vendored from the Lab

- `vendor/lab/lab-internal.openapi.json` — the Lab's internal API
  (`/internal/*`: credential introspection, accounts, Meilisearch targets)
  that the hosted engine calls with `LAB_SERVICE_TOKEN`. Owned by
  `meilisearch/lab` (`contracts/lab-internal.openapi.json`); never edit the
  vendored copy by hand.

`check-drift.sh` compares the vendored copy with the Lab's `main`:

```bash
contracts/check-drift.sh          # or: just check-contracts
contracts/check-drift.sh --fix    # re-vendor; or: just sync-contracts
```

The owner copy comes from `$LAB_SRC/contracts/` when `LAB_SRC` points at a
local Lab checkout, else from GitHub through `gh api` (the Lab repo is
private, so it needs `GH_TOKEN` or `LAB_REPO_TOKEN`). With neither, the check
prints a notice and exits 0. CI's `contracts` job runs it with the
`LAB_REPO_TOKEN` secret.
