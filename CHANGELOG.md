# Changelog

All notable changes to the Scrapix engine are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow SemVer. Releases are tagged `vX.Y.Z` by the release workflow (see
"Versioning, releases and deployment" in CLAUDE.md). The
SDKs in `sdks/` are versioned and tagged on their own (`sdk-*-vX.Y.Z`).

## [Unreleased]

### Added

- **Automated releases.** `scripts/prepare-release.sh` bumps the workspace
  version in `Cargo.toml` and `Cargo.lock` and the contract versions
  (`info.version` of `contracts/openapi.json` and
  `contracts/openapi.engine.json`, and `info(version)` in
  `bins/scrapix-api/src/openapi.rs`), and folds `[Unreleased]` into a dated
  section of this file, in one idempotent step. The "Prepare release"
  workflow runs it and opens the `chore(release): vX.Y.Z` PR; merging it tags
  the merge commit and runs `release.yml` (GitHub release with this file's
  notes, role images `ghcr.io/qdequele/scrapix-<role>:vX.Y.Z`, linux/amd64),
  which notifies `meilisearch/lab` (`scrapix-released`) when the
  `LAB_DISPATCH_TOKEN` secret is set. A new CI job, `Version sync`, fails
  when the version copies disagree.

### Changed

- The OpenAPI `info.version` of `contracts/openapi.json` and
  `contracts/openapi.engine.json` (and the `/openapi.json` the engine serves)
  is now the crate version, `0.3.0`; it was a fixed `0.1.0`. The Lab
  re-vendors `contracts/openapi.json` for this.
- `benches/Cargo.toml` takes the workspace version.

### Fixed

- The spec types a selector's `default` (`/scrape` and `/batch/scrape`
  `extract`) and `schema.json_ld` as any JSON value instead of an object, so
  clients that validate against it (the Lab's MCP tools) accept
  `"default": "none"` or a number, as the engine always did. The SDKs follow:
  `unknown` in TypeScript (was `Record<string, never>`), `Any` in Python.
- `/scrape` and `/batch/scrape` `ai.summary` / `ai.extract` run whatever the
  requested `formats`: without `markdown` or `content` they used to do
  nothing and return "could not be generated".
- `/scrape` `timeout_ms` bounds the default fetch (`timeout` error, 504). It
  only applied with custom `headers` or `cookies`.
- `metadata` comes from the whole page even with `include_selectors` (the
  title was `null` when they dropped `<head>`), and ignores the `itemprop`
  metas of microdata items: `keywords` was the last quote's tags on
  quotes.toscrape.com.
- A document's fallback title keeps a dot followed by digits:
  `arxiv.org/pdf/1706.03762` is titled `1706.03762`, not `1706`.
- `schema` items carry `@type` once (it was serialized twice).
- An `extract` selector's `attribute` field is read by the `text`, `first`
  and `list` modes (`{"mode": "list", "attribute": "src"}`), as documented;
  it was ignored.
- `/extract` globs without a slash before `*` (`/learn*`) include their
  prefix page (`/learn`).
- Docs: the request format is `rawhtml` (the response field `raw_html`),
  `blocks` items show their real fields, and the `extract` examples use
  `mode`, not `type`.
- `contracts/openapi.json` catches up with the engine: `features.documents`
  (`DocumentsConfig`) and `features.ocr` (`OcrConfig`), `pdf.max_pages`, and
  `pdf.extract_links`, which follows links inside PDFs (the spec still called it
  a no-op). The Lab re-vendors it, so its MCP crawl tools gain these options.
- `POST /scrape` with no `formats` returns `markdown`, `content` and `metadata`;
  the spec (both contracts) and the API reference said "all".
- The crawl docs no longer mark `index_uid` and `meilisearch` as required: the
  index name is derived from the first start URL, and behind the Meilisearch Lab
  a crawl without `meilisearch` writes to the team's default instance. They
  document `index_strategy` instead of the legacy `replace_index`.

[Unreleased]: https://github.com/qdequele/scrapix/compare/v0.5.0...HEAD
