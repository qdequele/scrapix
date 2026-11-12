# Changelog

All notable changes to the Scrapix engine are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow SemVer. Releases are tagged `vX.Y.Z` by the release workflow (see
"Versioning, releases and deployment" in CLAUDE.md). The
SDKs in `sdks/` are versioned and tagged on their own (`sdk-*-vX.Y.Z`).

## [Unreleased]

### Added

- **AI usage and cost on jobs and scrapes.** `GET /jobs/{id}` (crawl and
  extract jobs), `GET /extract/{id}`, `POST /scrape` and `POST /parse` carry
  `ai_usage`: the LLM calls made (AI summary, extraction, vision OCR), their
  tokens, and `cost_micro_usd`, the provider cost the hosted AI gateway
  reported for them (Lumen's `x-lumen-cost-micro-usd`, which needs a Lumen
  release that sends it). The gateway bills that cost, not the job's
  credits; the Lab console shows it on the job. Each crawled page reports
  its AI usage on `DocumentIndexed` (`ai_usage`), summed into the job's
  accounting snapshot. Batch scrape jobs do not report it yet.

- **Lightpanda renderer for browser crawls (prototype).** With
  `BROWSER_RENDER` and the new `LIGHTPANDA_URL` (and
  `LIGHTPANDA_CONNECTIONS`, default `8`), the crawler worker renders plain
  crawl pages on a running Lightpanda 1.0+ first, and re-renders in Chrome
  when Lightpanda fails, times out, meets a bot challenge or returns a page
  with almost no text. `/scrape` (screenshots, actions, mobile, stealth)
  stays on Chrome. In a 46-URL benchmark at 8 pages at once, Lightpanda
  rendered every page in 7 to 11 s against 38 to 58 s for Chrome, with
  about a tenth of the memory, and handed 5 pages to Chrome.
  `scrapix-crawler` gains the `browser-lightpanda` feature
  (`LightpandaRenderer`, enabled by the worker's `browser` feature) and the
  `lightpanda_vs_chrome` benchmark example.

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

- **A site paced by the adaptive delay is crawled at the rate that delay
  allows, whatever the dispatch period.** The frontier granted a domain one
  URL per dispatcher pass, and only once a full delay had passed since the
  last one, so the time between a request becoming due and the pass that
  sent it was lost: a 25 ms delay with a pass every 33 ms gave 30 pages/s
  instead of 40. The politeness stores now keep a schedule per domain and
  give that time back: a late pass sends the URLs it is owed in one call.
  The credit is 100 ms (`DOMAIN_SPACING_CREDIT_MS`, `0` restores the old
  spacing), so such a site gets at most `1 + 100 / delay` requests at once
  (5 at the 25 ms floor), never more than its free in-flight slots, and
  never more than `1 / delay` on average. Explicit delays are unchanged: a
  job's `per_domain_delay_ms` or `requests_per_second`, a robots
  `Crawl-delay`, `default_crawl_delay_ms` and any delay of 1 s or more stay
  a strict floor on the gap between two requests. A `Retry-After` pause
  banks no credit, and a 429 or an error drops it (SCR-90). Measured end to
  end on a local stack: a single site goes from 26 to 37 pages/s (24 with
  the credit at `0`), against a ceiling of 40 at the 25 ms floor.

- **Kafka producers no longer wait 20 ms before sending.** Every publish
  stage awaits its delivery report, so the old `linger.ms=20` added about
  20 ms to each one (about 40 ms per page in the crawler, one linger per
  frontier dispatch pass). The default is now `0`: on a local Redpanda an
  awaited send takes 0.12 ms instead of 22 ms, and sustained throughput is
  unchanged because batches fill by size. End to end, a crawl of 20 sites
  in one job runs at about 84 pages/s with `0` and 65 with `20`; a single
  site is paced by politeness and does not change. The new `KAFKA_LINGER_MS`
  (integer, 0 to 1000) sets it for every producer; an invalid value refuses
  to start.

- **The binaries can be built with mimalloc.** A new cargo feature,
  `mimalloc`, on each binary crate, off by default (`--features mimalloc`).
  On the parser benchmarks in a Linux container it is 3 to 4% faster on HTML
  parsing and 8 to 10% faster on Markdown conversion and the full parse
  pipeline. It is not the default because a full crawl is not faster with
  it (84 pages/s with and without, 20 sites in one job, macOS arm64) and
  the content worker's peak memory doubles (about 230 MB against 105 MB).

- **Markdown conversion is up to a third faster, with the same output.**
  The tags dropped from a page are found in one scan instead of one regex
  pass per tag, the Markdown cleanup skips the patterns that cannot match,
  and links on ordinary lines are resolved without the regex engine. On the
  parser benches: 29 to 37% less time for the crawl's conversion, 12 to 20%
  for `html_to_markdown`. The Markdown is byte-identical: the previous
  implementation is kept as a test reference and compared on 1,750 pages.

- **robots.txt costs one connection and one request per new origin.** The
  crawler worker fetches robots.txt through the page fetcher's DNS cache
  and connection pool, and the first URLs of an origin share a single
  robots.txt fetch instead of each fetching it. The body is read up to
  500 KiB (the rest is ignored, cut at a line end), and the cache drops
  expired entries and keeps at most 100,000 origins. What robots.txt
  allows, its TTL, `Crawl-delay` and the SSRF checks are unchanged. The
  request now also carries the fetcher's `Accept-Language` header.

- **The engine asks the Lab once per credential, not once per request.**
  When a cached Lab answer expired (every 30 s for a busy API key), every
  request in flight for that key called the Lab at the same time.
  Concurrent requests for the same key now share one Lab call, and a key in
  use is refreshed in the background from 80% of its `cache_ttl`. The
  guarantees are unchanged: a revoked key works for at most `cache_ttl`,
  answers are served stale for at most 5 minutes while the Lab is down, and
  `cache_ttl` 0 answers are fetched by every request. `/scrape`, `/map` and
  `/search` also record their usage event in one database round trip
  instead of three.

- **The crawler publishes a page's messages together.** Each message
  waited about 20 ms for its delivery report while the page held a worker
  slot, so a page with 100 links took about 2 s to publish. A page, its
  links (256 at a time) and its fetch feedback are now sent together, and
  sitemap entries 256 at a time. `PageCrawled` is still published after the
  page and its links (SCR-89).

- **A crawler worker publishes a link once per job.** Links repeated on
  every page (navigation, sidebar, footer) were sent to the frontier from
  each page and refused there as duplicates. A worker now skips the links
  it already sent for the job, so `links_published` and
  `UrlsDiscovered.count` are the links actually sent. A skipped link is
  sent again after 10 to 20 minutes, or by another worker; the frontier
  still decides what is a duplicate.

- **The frontier dispatches every running job concurrently.** One pass
  used to handle the jobs one after another, each waiting for its batch's
  sends, so ten single-site jobs shared about 40 URLs/s. A pass now runs up
  to 32 jobs at once (never the same job twice), and a batch asks for the
  politeness slots of up to 32 domains at once. After a grant, the domain's
  other URLs in the batch are parked without asking the politeness store
  again. Paused jobs and jobs leased by another instance are checked every
  200 ms, idle running jobs every 100 ms, instead of every 20 ms tick.
  Politeness rules and defaults are unchanged.

- **The content worker publishes page outcomes concurrently.**
  `DocumentIndexed`, `DocumentSkipped` and `DocumentFailed` go out with up
  to 256 sends in flight, each page acked when its own event is delivered.
  One awaited send per page capped a worker near 45 pages/s and delayed job
  completion. Outcome events of different pages can arrive out of order.

- **The content worker parses a page once, off the async runtime.** The
  job's schema options, custom selectors and block splitting reuse the
  parser's DOM, Markdown is converted only when the page's features keep
  it, and the parse runs on blocking threads (one per core). Language
  detection reads the first 8 KiB of the content. The crawler worker parses
  pages over 32 KiB for links on blocking threads too, and URL pattern
  matching no longer allocates on each call.

- **Meilisearch flushes are faster and isolated per tenant.** The periodic
  flush runs every `FLUSH_INTERVAL_MS` (new, content worker, default
  `1000`; it was a fixed 5 s) and per storage, so a slow or unreachable
  Meilisearch only delays its own tenant. A document request is cut at
  `BATCH_SIZE` documents or 20 MiB, whichever comes first, and a request
  refused for its size (413) is split in two and retried instead of failing
  every document in it. A storage idle for 10 minutes is dropped from
  memory.

- **Browser renders skip images, fonts, audio and video.** The Chrome
  renderer's `block_images`, `block_fonts` and `block_stylesheets` options
  were never applied; they now are, with a new `block_media`. The crawler
  worker and the API (`/scrape`, `/map` with `render_js`) block images,
  fonts and media. A render with a screenshot or page actions loads
  everything, and stylesheets always load. Page setup before navigation is
  sent in one batch instead of one CDP round trip after the other.

- **The API's browser pool is configurable, and `/map` no longer starves
  it.** `API_BROWSER_CONCURRENCY` (new, default `5`) sets how many pages
  the API's browser renders at once. `/map` with `render_js` runs at most
  the pool minus a reserve for `/scrape` (a fifth of it, at least one
  page) instead of 50 renders queued on 5 pages, and its 30 s render
  timeout starts when the render gets its page.

- **`/scrape` fails faster on a page that is down.** A page answering 429
  or 5xx is fetched twice (one retry) instead of four times with 3.5 s of
  backoff, in `/scrape`, `/batch/scrape` and `/extract`. `timeout_ms` now
  bounds the whole fetch for requests with `headers` or `cookies` too: a
  slower page is a `timeout` (504), where it used to be reported as
  `fetch_error` (502) after every attempt.

- **`/scrape` does its HTML work once, off the async runtime.** The page is
  parsed once instead of once per format, Markdown is converted once when
  `markdown` and `content` are both requested, and the stage runs on
  blocking threads. Responses are unchanged.

- **`/scrape` requests with `headers` reuse the shared HTTP client**
  (connection pool, DNS cache) instead of building two clients per request.
  They no longer keep a cookie store, so a cookie set along a redirect
  chain is not sent back on the next hop. Requests with `cookies` or a
  `Cookie` header keep their own client and cookie store.

- **Responses are gzipped.** Bodies of 1 KB and more are compressed when
  the client sends `Accept-Encoding: gzip`, `/metrics` included. Event
  streams and WebSockets are not.

- **`/batch/scrape` and `/extract` scrape on separate tasks**, so pages
  keep being fetched while finished results are stored. **`/search` uses
  one pooled client with timeouts**: a Meilisearch that does not answer
  within 30 s (5 s to connect) is a `timeout` (504) instead of a request
  that never returns. **`/map` with `depth`** fetches the start page while
  it reads the sitemaps.

- **Single-site crawls up to about 10× faster: the per-domain delay
  adapts to the site.** It used to be a fixed 250 ms floor
  (`DOMAIN_DELAY_MS`), capping one site at about 4 pages/s; a job could
  only raise it. It now starts at `DOMAIN_DELAY_MS` and, after each
  successful fetch, moves halfway toward the fetch time divided by
  `CONCURRENT_PER_DOMAIN` (dropping at most ×0.9 per success), down to the
  new `MIN_DOMAIN_DELAY_MS` (default `25`). A site answering in 200 ms
  reaches about 40 pages/s, the ceiling the 20 ms `DISPATCH_INTERVAL_MS`
  also sets (one URL per domain per dispatch). A site that slows down gets
  a longer delay, even above `DOMAIN_DELAY_MS`. 429s, `Retry-After`, error
  streaks and robots.txt `Crawl-delay` still slow it down as before.
  `CONCURRENT_PER_DOMAIN` defaults to `8` (was `4`). Set
  `MIN_DOMAIN_DELAY_MS` to `DOMAIN_DELAY_MS` to never go below the old
  delay. The crawler reports the fetch time in `FetchFeedback`
  (`fetch_ms`); feedback from older crawlers keeps the old behavior.

- **A job's `rate_limit.per_domain_delay_ms` defaults to `0`** (was `100`):
  the adaptive delay applies unless the job sets one, and an explicit value
  is still a floor. As a side effect, `rate_limit.default_crawl_delay_ms`
  now applies to jobs that set no delay or rate, as documented; the
  implicit `100` used to switch it off.

- **The frontier sends URLs to the crawlers concurrently.** It used to
  wait for each URL's bus send (producer linger plus broker ack) before
  the next, capping one frontier at about 45 URLs/s across every job.
  Politeness slots are still taken one URL at a time, but a batch's sends
  now go out together (about 19,000 sends/s measured on a local Redpanda).
  A graceful shutdown still sends the URLs already granted a slot and puts
  the rest back; a failed or abandoned send puts its URL back first, then
  frees its slot (skipped after the shutdown hard stop: the slot expires).

- The OpenAPI `info.version` of `contracts/openapi.json` and
  `contracts/openapi.engine.json` (and the `/openapi.json` the engine serves)
  is now the crate version, `0.3.0`; it was a fixed `0.1.0`. The Lab
  re-vendors `contracts/openapi.json` for this.
- `benches/Cargo.toml` takes the workspace version.
- **`/map` is much faster.** Each page is fetched once at most (with
  `depth > 0`, a page was fetched once for its title and again for its
  links), and only its `<head>` is kept and parsed when its links are not
  followed. Pages run from one queue instead of depth by depth, so one slow
  page no longer holds up its level. Concurrency starts at 32, grows to 100
  while the site keeps up and halves on `429`/`503`/timeouts (it was a fixed
  50). A page fetched only for its title gets 8 s and no retry (it was 30 s
  and 3 retries); a page whose links are followed keeps 30 s. On a local
  site with 2% of pages taking 20 s, a depth-2 map went from 60 s (cut by
  the deadline) to 20 s, and a depth-3 map found 5,000 URLs in 21 s instead
  of 931 in 60 s. Sitemaps, and the children of a sitemap index, are fetched 8 at
  a time (for crawls and `/extract` globs too), and `/map` stops reading
  them at about twice its `limit` instead of 100,000 URLs.

### Fixed

- **Page text that looks like deeply nested Markdown links no longer
  crashes the process.** Resolving links recursed once per link nested in a
  link text, so a 160 KB page repeating `[![a](b)` overflowed the stack and
  aborted the content worker (or the API on `/scrape`), which then got the
  page again. The nesting is followed 32 levels deep; past that the text of
  a link is kept as it is.

- **A page too large to publish fails instead of stalling its job.** A
  document between about 7.5 and 10 MB is over the 10 MiB message limit
  once base64-encoded, and so is HTML that grows past it when escaped. Its
  send failed every time, the message was never acknowledged and the job
  ended `FailStalled`. Such a page is now a failed page (`page too large to
  publish`) and the crawl goes on.

- **`exclude_selectors` removes every matched element.** An element whose
  HTML was not written the way a parser serializes it (unquoted attributes,
  unclosed tags, tables without `<tbody>`) was silently kept. The page is
  also parsed once instead of once per selector.

- **A crashed scrape no longer hangs its batch or extract job.** The URL
  gets an `internal_error` result and the job finishes.

- **Re-crawling an index failed on sites that send `ETag` or
  `Last-Modified`.** On an `update` crawl, an unchanged page answered `304`
  and its links were never followed: a second crawl of books.toscrape.com
  failed with "No page could be crawled (0 failures)", and on other sites
  changed pages below an unchanged one were never reached. The crawl history
  now keeps each page's links, and a `304` page publishes them again; a
  crawl whose pages are all unchanged completes. The first re-crawl after
  this release fetches pages in full once, to store their links, and so
  does the first re-crawl after a config's link filters (include/exclude,
  `allowed_domains`) change, so widened patterns find their new pages.
- **A start URL that redirected crawled one page.** The include pattern
  generated from a start URL with a path used it as typed, so after
  `https://meilisearch.com/docs` redirected to `www.`, every link was
  filtered out. The generated patterns accept `http`/`https` and the host
  with or without `www.`, and drop a trailing page name (`/index.html`). A
  root start URL listed next to start URLs with a path now crawls its host;
  its links were all filtered out.
- **One page was crawled twice under two spellings.** Start URLs and sitemap
  entries skipped the normalization links get, so `/post/` from a sitemap
  and `/post` from a link were two pages, each counted against `max_pages`
  and billed. A redirect target is also recorded as seen, so a page reached
  by a redirect and by a link is fetched once. A page known only from a
  sitemap, listed with a trailing `/` on a server that answers both forms
  without redirecting, is now indexed under the form without it: on an
  `update` crawl the old copy stays until a `replace` crawl.
- **Redirects bypassed the crawl's scope.** A link that redirected to
  another host, outside the include/exclude patterns or to a path
  robots.txt disallows was fetched and indexed. It is now skipped with the
  reason (start URLs may still redirect anywhere).
- **`<base href>` was ignored** when resolving links, in crawls and in
  `/map`.
- **Sitemap discovery in crawls forced `https` and dropped the port**, so
  http-only sites and sites on a non-default port got no sitemap. (`/map`
  was fixed earlier.)
- **Pages blocked by robots.txt and unsupported content types were
  failures.** They are now skipped pages with the reason (`page_skipped`),
  not `errors`, so the console no longer lists them under failed pages.

- **`block_split` dropped `custom` and `schema`.** Custom selectors and
  JSON-LD were extracted onto the page, but only the block documents were
  indexed, and they carried neither: a crawl of the Meilisearch docs with
  `schema` on indexed 241 blocks and no `schema`. Every block now carries
  them, like `metadata`. Block documents also get unique ids when a page
  repeats a heading `id` (the later block overwrote the earlier one) or has
  an empty one.
- **`custom_selectors` kept only the first match.** `".breadcrumb li"` gave
  `"Home"` and dropped the rest, and a list of selectors was run as one CSS
  selector (`".a, .b"`), so it gave the first match in page order. A
  selector now returns every match (a string for one, an array for
  several), a list combines the matches of all its selectors in list order,
  an invalid selector in a list is skipped instead of failing the field, and
  an element without text (`<meta>`) gives its `content` attribute. A field
  that matches several elements is now an array where it was a string:
  update anything that sorts on it or expects a string.
- **A `replace` crawl reported the whole index's document count as its
  own.** After its cleanup, `pages_indexed` was overwritten with the index's
  `numberOfDocuments`, documents of other crawls sharing the index included
  (a 1-page blog crawl reported 20). It keeps the job's own count and only
  lowers it when Meilisearch holds fewer of the job's documents. The "index
  may be incomplete" warning only counts document tasks that failed since
  the job started (it counted every failure the index ever had).
- **`meilisearch.primary_key` other than `uid` emptied the index.** Crawled
  documents only carry `uid`, so Meilisearch refused every batch after the
  job had reported the pages indexed. Any other value is now a `400`.
- **`documents_sent` counted pages.** With `block_split` it was 14 for 241
  documents written. It now counts documents, as documented; the
  `page_indexed` webhook and the `DocumentIndexed` event carry `documents`
  (events from older workers count 1).
- **`index_only` and the features' `include_pages` / `exclude_pages` matched
  the URL before redirects**, not the URL the page is indexed under.
- **Block titles carried zero-width characters** (`… - ​How it works`
  on Mintlify sites). Headings drop U+200B–U+200D, U+2060 and U+FEFF.
- **With `ENABLE_DEDUP`, near-duplicates were remembered across jobs**, so a
  page that moved to a new URL was skipped as a copy of its old one (and a
  `replace` crawl then deleted the old one). Detection is now per job.
- **Crawled `content` was not plain text.** Headings, list items and code
  kept markdown markers (`# Herman Melville - Moby-Dick`). `/scrape`'s
  `content` with `only_main_content: false` was the page's markdown; it is
  plain text too.
- **Markdown links stayed relative.** Crawled and scraped `markdown` had
  `[Login](/login)`; links and images now resolve against the page's
  `<base href>` or URL. A link without an `href` (`<a class="tag">`) is kept
  as its text instead of `[change]()`.

- **The link-local URL block now applies on Lightpanda.** Lightpanda 1.0
  only accepts the newer `Network.setBlockedURLs` form (`urlPatterns`), so
  the block (169.254.0.0/16, fe80::/10, the cloud metadata endpoints) failed
  on every Lightpanda render and logged `Could not block link-local URLs on
  the page`. The Lightpanda renderer now sends that form. The target and
  final-URL checks, and Lightpanda's `--block-private-networks`, were in
  place meanwhile.

- **`/map` ran past its 60 s limit.** The link-following levels never
  checked it, and fetches kept running after it. Sitemap discovery now
  counts toward it too, and pending fetches are cancelled when it is hit.
- **`/map` resolved relative links against the map's URL**, not the page
  they were on: `href="intro"` on `/docs/` became `/intro`.
- **Sitemap discovery dropped the port** of the site's URL, so it found no
  sitemap on a site served on a non-default port.
- **Replace crawls sharing an index deleted each other's documents.** After a
  `"index_strategy": "replace"` crawl, every document of the index not
  indexed by that job was deleted (`_crawl_job_id != <job>`). Two saved
  configs writing to one index (a blog crawl and a docs crawl) wiped each
  other every day. Documents now carry `_crawl_scope`, an id of the crawl's
  `start_urls` (scheme, `www.` and a trailing `/` ignored), and the cleanup
  only deletes `_crawl_scope = <this crawl> AND _crawl_job_id != <job>`.
  Documents indexed before this release have no `_crawl_scope`; the cleanup
  deletes those only when their `url` lies under one of the crawl's start
  URLs. `_crawl_scope` is made filterable like `_crawl_job_id` (also by the
  cleanup itself on an index that lacks it), and a failed cleanup task now
  fails the job instead of passing silently.
- **A `*` at the start of a URL pattern matched nothing.** A single `*`
  does not cross `/`, and every URL starts with `scheme://host`, so
  `*/blog/*` excluded every link and a crawl with that `include` never left
  its start page. A leading `*` now matches like `**` (as a trailing one
  already did).
- **`http://` and `https://` of one page were crawled and indexed twice.**
  The frontier now treats both schemes of a URL as one page; a redirect from
  `https` to `http` on the same host keeps the `https` URL (some sites behind
  a TLS proxy redirect `https://h/x` to `http://h/x/`, so every page of an
  `https` crawl was indexed as `http`); and a same-host `http://` link on an
  `https` page is followed as `https`.
- **Browser crawls indexed almost nothing.** The renderer took the content
  type and status of whatever request CDP reported for the navigation, which
  was often one of the page's subresources (`text/css`, `font/woff2`), so the
  content worker skipped the page as "unsupported content type". A
  `crawler_type: browser` crawl of quotes.toscrape.com crawled 4 pages and
  indexed 0. Only the document's own response counts now, and without one
  the page's `document.contentType` is used.
- **URL patterns that mix `**` and `*`.** After a `**`, a `*` was compared
  literally, so `**/page/*`, `**/login*`, `**/*.pdf` and `**?lang=*` (the
  last two from the docs) never matched: an `index_only` of `**/page/*`
  indexed nothing and an `exclude` of `**/login*` excluded nothing. Patterns
  are now matched as globs over the whole URL (`**` crosses `/`, `*` does
  not). A trailing `*` still matches like `**`. A pattern that ends in a
  literal must now end the URL: `https://a.com/*/feed` no longer takes
  `https://a.com/news/feeds`. This applies to `include`, `exclude`,
  `index_only`, the features' `include_pages` / `exclude_pages` and
  `/extract` globs.
- **`/scrape` custom `headers` with a browser.** With `render_js`, `mobile`,
  `actions` or a screenshot, the headers were not sent; they are now, minus
  the same hop-by-hop headers the HTTP path blocks. `/batch/scrape` and
  `/extract` follow.
- **`/scrape` `timeout_ms` with a browser.** It only bounded the HTTP fetch; a
  rendered page could take as long as it liked. It now bounds the render too
  (`timeout`, 504), with `actions` keeping their 30 s budget on top, and the
  page is still closed.
- **Crawl `markdown` was plain text.** The content worker converted
  readability's text output, so `markdown` repeated `content`. It is now
  built from the page's main content HTML, with headings, links, lists and
  tables, like `/scrape`.
- **Crawled titles** collapse whitespace (`Poetry | Books to Scrape`, was
  `Poetry | \n     Books to Scrape`).
- **Why a crawled page was not indexed.** The content worker's
  `document_skipped` and `document_failed` events are now kept in the job's
  event history (`GET /job/{id}/events/history`) as `page_skipped` (with the
  reason) and `page_failed`.

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
