# Scrapix Python SDK

Official Python client for the [Scrapix](https://scrapix.meilisearch.com) API:
scrape pages, map sites, run distributed crawls, batch scrapes and structured
extractions, and read their results. Sync and async (`asyncio`/`trio`)
clients, typed pydantic models generated from the OpenAPI spec, retries with
`Retry-After` support, and a job watcher.

```bash
pip install scrapix
```

Requires Python 3.9+. Runtime dependencies: `httpx`, `pydantic` (v2), `anyio`.

## Quickstart

```python
from scrapix import Scrapix

client = Scrapix(api_key="sk_live_...")  # or set SCRAPIX_API_KEY

# One page, synchronously
page = client.scrape("https://example.com", formats=["markdown", "metadata"])
print(page.markdown)

# A whole site: start a crawl, wait for it, get every document
result = client.crawl_and_wait(
    "https://docs.example.com",
    max_pages=200,
    on_progress=lambda job: print(job.status.value, job.pages_crawled),
)
print(result.status, len(result.documents))
for doc in result.documents:
    print(doc.url, (doc.markdown or "")[:80])
```

## Configuration

| Argument | Environment variable | Default |
|----------|----------------------|---------|
| `api_key` | `SCRAPIX_API_KEY` | none (a self-hosted engine without auth needs none) |
| `base_url` | `SCRAPIX_API_URL` | `https://scrapix.meilisearch.dev` |
| `timeout` | | `60` seconds per request (`None`: no timeout) |
| `max_retries` | | `2` |

API keys (`sk_live_...` / `sk_test_...`) are sent in the `X-API-Key` header.
Any other value is treated as an OAuth access token and sent as
`Authorization: Bearer <token>`.

```python
client = Scrapix(base_url="http://localhost:8080", timeout=120, max_retries=5)
```

Pass `http_client=httpx.Client(...)` to use your own client (proxies, custom
transport). `Scrapix` and `AsyncScrapix` are context managers.

## Methods

Request options are keyword arguments named exactly like the API fields (see
the [API reference](https://scrapix.meilisearch.com/docs/api-reference/overview)).
You can also pass a request model (`ScrapeRequest`, `CrawlConfig`, ...) or a
plain `dict` as the first argument.

| Method | Endpoint |
|--------|----------|
| `scrape(url, **options)` | `POST /scrape` |
| `map(url, **options)` | `POST /map` |
| `search(url, q, **options)` | `POST /search` |
| `crawl(start_urls, **config)` | `POST /crawl` (returns immediately) |
| `crawl_sync(start_urls, include_results=False, **config)` | `POST /crawl/sync` (blocks) |
| `batch_scrape(urls, **options)` | `POST /batch/scrape` |
| `extract(urls, prompt=..., schema=..., **options)` | `POST /extract` |
| `get_extract(job_id)` | `GET /extract/{id}` |
| `get_job(job_id)` | `GET /job/{id}/status` |
| `list_jobs(limit=, offset=)` | `GET /jobs` |
| `cancel_job` / `pause_job` / `resume_job` | `DELETE /job/{id}`, `POST /job/{id}/pause`, `POST /job/{id}/resume` |
| `job_results(job_id, limit=, cursor=)` | `GET /job/{id}/results` (one page) |
| `iter_job_results(job_id, wait=False)` | every result, following the cursor |
| `wait_for_job(job_id, poll_interval=2, timeout=None, on_progress=None)` | polls until terminal |
| `iter_job_status(job_id)` | yields each polled status |
| `crawl_and_wait` / `batch_scrape_and_wait` / `extract_and_wait` | create + wait + collect results |
| `health()` | `GET /health` |

### Jobs

Crawls, batch scrapes and extractions are asynchronous jobs. A job ends in
`completed`, `failed` or `cancelled`; `paused` is not terminal.

```python
created = client.batch_scrape(["https://a.com", "https://b.com"], formats=["markdown"])

# Watch progress
for status in client.iter_job_status(created.job_id, poll_interval=1):
    print(status.status.value, status.pages_crawled, status.errors)

# Or block, with a timeout (raises JobTimeoutError; the job keeps running)
final = client.wait_for_job(created.job_id, timeout=600)

# Every result, page by page (100 per request)
for item in client.iter_job_results(created.job_id):
    print(item.index, item.url, item.success)
```

`iter_job_results` on a running job stops once it has caught up with the
results available so far; pass `wait=True` to keep polling until the job is
terminal and every result was read.

`wait_for_job` and the `*_and_wait` helpers return failed or cancelled jobs
instead of raising: check `result.succeeded` / `result.status`.

```python
result = client.extract_and_wait(
    ["https://example.com/blog/*"],
    prompt="The title and author of each post",
)
print(result.data)          # the extraction
print(result.extract.sources)
```

### Async

```python
import asyncio
from scrapix import AsyncScrapix

async def main() -> None:
    async with AsyncScrapix() as client:
        page = await client.scrape("https://example.com")
        async for item in client.iter_job_results("job-id"):
            print(item.url)
        result = await client.crawl_and_wait(["https://docs.example.com"], max_pages=50)

asyncio.run(main())
```

`on_progress` may be a regular function or a coroutine function.

## Errors

Every error derives from `scrapix.ScrapixError`. Non-2xx responses raise an
`APIStatusError` subclass carrying `status_code`, the API's `code` and
`message`, `details` and the raw `body`:

| Status | Exception |
|--------|-----------|
| 400 | `BadRequestError` |
| 401 | `AuthenticationError` |
| 402 | `InsufficientCreditsError` |
| 403 | `PermissionDeniedError` |
| 404 | `NotFoundError` |
| 409 | `ConflictError` |
| 422 | `UnprocessableEntityError` |
| 429 | `RateLimitError` (`retry_after` seconds) |
| 5xx | `InternalServerError` |

Network failures raise `APIConnectionError` (`APITimeoutError` for timeouts).

```python
from scrapix import NotFoundError, RateLimitError

try:
    client.get_job("unknown")
except NotFoundError as exc:
    print(exc.status_code, exc.code, exc.message)
```

## Retries

429 responses are retried (up to `max_retries`), waiting for the delay in the
`Retry-After` header (capped at 60 s). Requests that are safe to repeat
(reads, `scrape`, `map`, `search`) are also retried on 408/5xx, timeouts and
network errors, with exponential backoff. Requests that create or change a
job (`crawl`, `crawl_sync`, `batch_scrape`, `extract`, cancel/pause/resume)
are only retried on 429 and on connection failures (the request never
reached the API), so a job is never started twice.

## Models

`scrapix.models` exposes a pydantic model for every schema of the API,
generated from [`contracts/openapi.json`](../../contracts/openapi.json).
Models accept unknown fields, so a newer API does not break an older SDK. If
a response does not match its model, it is still returned (unvalidated) and a
warning is logged on the `scrapix` logger. Fields that clash with pydantic
(`schema`) are exposed as `schema_` and keep the API name as their alias.

## Development

```bash
# from the repository root
just sdk-generate   # regenerate src/scrapix/_generated/models.py from the spec
just sdk-check      # fail if the generated code is stale
cd sdks/python
uv run --group dev pytest
uv run --group dev ruff check . && uv run --group dev mypy
uv build
```

Do not edit `src/scrapix/_generated/`: it is overwritten by `just sdk-generate`.

## License

MIT
