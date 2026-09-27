# Scrapix TypeScript SDK

Official TypeScript/JavaScript client for the [Scrapix](https://scrapix.meilisearch.com)
API: scrape pages, map sites, run distributed crawls, batch scrapes and
structured extractions, and read their results. Zero runtime dependencies
(uses the global `fetch`), ESM + CommonJS, types generated from the OpenAPI
spec, retries with `Retry-After` support, and a job watcher.

```bash
npm install scrapix
```

Requires Node.js 18+ (or any runtime with `fetch`: Deno, Bun, edge runtimes).

## Quickstart

```ts
import { Scrapix } from "scrapix";

const scrapix = new Scrapix({ apiKey: "sk_live_..." }); // or set SCRAPIX_API_KEY

// One page, synchronously
const page = await scrapix.scrape("https://example.com", { formats: ["markdown", "metadata"] });
console.log(page.markdown);

// A whole site: start a crawl, wait for it, get every document
const { job, documents } = await scrapix.crawlAndWait(
  { start_urls: ["https://docs.example.com"], max_pages: 200 },
  { onProgress: (s) => console.log(s.status, s.pages_crawled) },
);
console.log(job.status, documents.length);
```

## Configuration

| Option | Environment variable | Default |
|--------|----------------------|---------|
| `apiKey` | `SCRAPIX_API_KEY` | none (a self-hosted engine without auth needs none) |
| `baseUrl` | `SCRAPIX_API_URL` | `https://scrapix.meilisearch.dev` |
| `timeoutMs` | | `60000` per request (`0` disables it) |
| `maxRetries` | | `2` |
| `headers` | | extra headers for every request |
| `fetch` | | `globalThis.fetch` |

API keys (`sk_live_...` / `sk_test_...`) are sent in the `X-API-Key` header.
Any other value is treated as an OAuth access token and sent as
`Authorization: Bearer <token>`.

Every method takes an optional last `RequestOptions` argument
(`{ timeoutMs, maxRetries, signal }`).

## Methods

Request bodies use the API's field names (snake_case) and are fully typed.

| Method | Endpoint |
|--------|----------|
| `scrape(url, params?)` | `POST /scrape` |
| `map(url, params?)` | `POST /map` |
| `search(url, q, params?)` | `POST /search` |
| `crawl(config)` | `POST /crawl` (returns immediately) |
| `crawlSync(config, { include_results, results_limit })` | `POST /crawl/sync` (blocks) |
| `batchScrape(request)` | `POST /batch/scrape` |
| `extract(request)` / `getExtract(jobId)` | `POST /extract`, `GET /extract/{id}` |
| `getJob(jobId)` | `GET /job/{id}/status` |
| `listJobs({ limit, offset })` | `GET /jobs` |
| `cancelJob` / `pauseJob` / `resumeJob` | `DELETE /job/{id}`, `POST /job/{id}/pause`, `POST /job/{id}/resume` |
| `jobResults(jobId, { limit, cursor })` | `GET /job/{id}/results` (one page) |
| `iterJobResults(jobId, { wait })` | async iterator over every result |
| `watchJob(jobId)` | async iterator over each polled status |
| `waitForJob(jobId, { pollIntervalMs, timeoutMs, onProgress })` | resolves when terminal |
| `crawlAndWait` / `batchScrapeAndWait` / `extractAndWait` | create + wait + collect results |
| `health()` | `GET /health` |

### Jobs

Crawls, batch scrapes and extractions are asynchronous jobs. A job ends in
`completed`, `failed` or `cancelled`; `paused` is not terminal.

```ts
const { job_id } = await scrapix.batchScrape({
  urls: ["https://a.com", "https://b.com"],
  formats: ["markdown"],
});

for await (const status of scrapix.watchJob(job_id, { pollIntervalMs: 1000 })) {
  console.log(status.status, status.pages_crawled);
}

for await (const item of scrapix.iterJobResults(job_id)) {
  console.log(item.index, item.url, item.success);
}
```

`iterJobResults` on a running job stops once it has caught up with the
results available so far; pass `{ wait: true }` to keep polling until the
job is terminal and every result was read.

`waitForJob` and the `*AndWait` helpers resolve with failed or cancelled jobs
instead of rejecting: check `job.status`. With `timeoutMs` they reject with a
`JobTimeoutError` (the job keeps running server-side).

```ts
const result = await scrapix.extractAndWait({
  urls: ["https://example.com/blog/*"],
  prompt: "The title and author of each post",
});
console.log(result.extract.data);
```

## Errors

Every error extends `ScrapixError`. Non-2xx responses reject with an
`APIError` subclass carrying `status`, the API's `code`, `apiMessage`,
`details`, the raw `body` and `headers`:

| Status | Error |
|--------|-------|
| 400 | `BadRequestError` |
| 401 | `AuthenticationError` |
| 402 | `InsufficientCreditsError` |
| 403 | `PermissionDeniedError` |
| 404 | `NotFoundError` |
| 409 | `ConflictError` |
| 422 | `UnprocessableEntityError` |
| 429 | `RateLimitError` (`retryAfter` seconds) |
| 5xx | `InternalServerError` |

Network failures reject with `APIConnectionError` (`APITimeoutError` for
timeouts, `APIUserAbortError` when your `signal` aborts).

```ts
import { NotFoundError } from "scrapix";

try {
  await scrapix.getJob("unknown");
} catch (error) {
  if (error instanceof NotFoundError) console.log(error.status, error.code, error.apiMessage);
}
```

## Retries

429 responses are retried (up to `maxRetries`), waiting for the delay in the
`Retry-After` header (capped at 60 s). Requests that are safe to repeat
(reads, `scrape`, `map`, `search`) are also retried on 408/5xx, timeouts and
network errors, with exponential backoff. Requests that create or change a
job (`crawl`, `crawlSync`, `batchScrape`, `extract`, cancel/pause/resume)
are only retried on 429, so a job is never started twice.

## Types

All API schemas are exported: `ScrapeRequest`, `ScrapeResponse`,
`CrawlConfig`, `JobStatusResponse`, `JobResultItem`, ..., and
`Schemas["AnySchemaName"]` for the rest. They are generated from
[`contracts/openapi.json`](../../contracts/openapi.json) by
[openapi-typescript](https://openapi-ts.dev).

## Development

```bash
# from the repository root
just sdk-generate   # regenerate src/generated/schema.ts from the spec
just sdk-check      # fail if the generated code is stale
cd sdks/typescript
npm ci
npm run typecheck && npm test
npm run build
```

Do not edit `src/generated/`: it is overwritten by `just sdk-generate`.

## License

MIT
