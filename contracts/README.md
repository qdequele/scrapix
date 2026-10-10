# Scrapix API Contracts

The contracts the Scrapix engine owns, plus the two it vendors from the
Meilisearch Lab. The Lab (Rails control plane + console) lives in
[`meilisearch/lab`](https://github.com/meilisearch/lab), which vendors a byte
copy of `openapi.json` and drift-checks it
(`openapi.engine.json` is not vendored); the Lab's contract test suite lives
there too.

## Engine-owned files

Change these only on an intentional, reviewed contract change — for the
vendored `openapi.json`, the Lab's drift check fails until it re-vendors it.

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

## Vendored from the Lab

- `vendor/lab/lab-internal.openapi.json` — the Lab's internal API
  (`/internal/*`: credential introspection, accounts, Meilisearch targets)
  that the engine calls with its `LAB_INSTANCE_ID` / `LAB_INSTANCE_SECRET`.
  Owned by `meilisearch/lab` (`contracts/lab-internal.openapi.json`); never
  edit the vendored copy by hand.
- `vendor/lab/lab-events.schema.json`: the events contract (spec: Lab
  platform contract v2 §4) every engine reports against at
  `POST {LAB_URL}/internal/events`: `usage.recorded` carries raw `units`
  (including `feature_pages`, the per-feature surcharge) and
  `provider_cost_micro_usd` (always 0 from Scrapix: the engine knows token
  counts, not provider prices). For the transition release Scrapix also
  sends the deprecated `credits` (its pre-v2 price,
  `bins/scrapix-api/src/legacy_credits.rs`), which the Lab debits as
  authoritative for product `scrapix`; both go next release. Owned by
  `meilisearch/lab` (`contracts/lab-events.schema.json`).

Both are byte copies of the Lab's contract v2 at commit
`42c282ce6005b14b1705711680fa70c0858e78ac` (meilisearch/lab#17). Until that
PR merges, the Lab's `main` still has v1 (and no `lab-events.schema.json`), so
CI pins the drift check to that commit with `LAB_REF`.

`check-drift.sh` compares the vendored copies with the Lab's `main`, or with
`$LAB_REF` (a branch, tag or commit) when set:

```bash
contracts/check-drift.sh          # or: just check-contracts
contracts/check-drift.sh --fix    # re-vendor; or: just sync-contracts
LAB_REF=<sha> contracts/check-drift.sh   # compare with a Lab branch/commit
```

The owner copy comes from `$LAB_SRC/contracts/` when `LAB_SRC` points at a
local Lab checkout, else from GitHub through `gh api` (the Lab repo is
private, so it needs `GH_TOKEN` or `LAB_REPO_TOKEN`). With neither, the check
prints a notice and exits 0. CI's `contracts` job runs it with the
`LAB_REPO_TOKEN` secret.
