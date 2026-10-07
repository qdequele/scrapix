"""Asynchronous Scrapix client (asyncio or trio, via anyio)."""

from __future__ import annotations

import time
from collections.abc import AsyncIterator, Awaitable, Mapping
from typing import Any, Callable, Optional, Union

import anyio
import httpx

from ._base import (
    CRAWL_SYNC_TIMEOUT,
    DEFAULT_MAX_RETRIES,
    DEFAULT_POLL_INTERVAL,
    DEFAULT_RESULTS_PAGE_SIZE,
    DEFAULT_TIMEOUT,
    NOT_GIVEN,
    Body,
    ExtractJobResult,
    JobResult,
    build_body,
    decode_json,
    extract_path,
    is_terminal,
    job_path,
    parse_model,
    parse_retry_after,
    resolve_config,
    retry_delay,
    should_retry,
    split_first,
)
from ._errors import APIConnectionError, APITimeoutError, JobTimeoutError, error_from_response
from ._generated.models import (
    BatchScrapeResponse,
    CrawlSyncResponse,
    CreateCrawlResponse,
    CreateExtractResponse,
    ExtractStatusResponse,
    HealthResponse,
    JobResultItem,
    JobResultsResponse,
    JobStatusResponse,
    MapResponse,
    ScrapeResponse,
)

__all__ = ["AsyncScrapix"]

# Indirection so tests can replace sleeping.
_sleep = anyio.sleep

ProgressCallback = Callable[[JobStatusResponse], Union[None, Awaitable[None]]]


class AsyncScrapix:
    """Asynchronous client for the Scrapix API.

    Args:
        api_key: an API key (``sk_live_...``, sent as ``X-API-Key``) or an
            OAuth access token (sent as ``Authorization: Bearer``). Defaults
            to the ``SCRAPIX_API_KEY`` environment variable. May be omitted
            for a self-hosted engine running without authentication.
        base_url: API root. Defaults to ``SCRAPIX_API_URL``, then
            ``https://scrapix.meilisearch.dev``.
        timeout: per-request timeout in seconds (``None``: no timeout).
        max_retries: retries on 429 (honoring ``Retry-After``) and, for
            requests that are safe to repeat, on 408/5xx and network errors.
            Requests that create or change a job are only retried on 429 and
            on connection failures (the request never reached the API).
        headers: extra headers sent with every request.
        http_client: a pre-configured ``httpx.AsyncClient`` (proxies, transport,
            ...). It is not closed by :meth:`close` unless created here.
    """

    def __init__(
        self,
        api_key: Optional[str] = None,
        *,
        base_url: Optional[str] = None,
        timeout: Optional[float] = DEFAULT_TIMEOUT,
        max_retries: int = DEFAULT_MAX_RETRIES,
        headers: Optional[Mapping[str, str]] = None,
        http_client: Optional[httpx.AsyncClient] = None,
    ) -> None:
        self._config = resolve_config(api_key, base_url, timeout, max_retries, headers)
        self._owns_client = http_client is None
        self._http = http_client if http_client is not None else httpx.AsyncClient()

    @property
    def base_url(self) -> str:
        return self._config.base_url

    async def close(self) -> None:
        """Close the underlying HTTP client (if this instance created it)."""
        if self._owns_client:
            await self._http.aclose()

    async def __aenter__(self) -> AsyncScrapix:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    # ------------------------------------------------------------------
    # Transport
    # ------------------------------------------------------------------

    async def _request(
        self,
        method: str,
        path: str,
        *,
        json: Any = None,
        params: Optional[Mapping[str, Any]] = None,
        timeout: Any = NOT_GIVEN,
        idempotent: bool = True,
    ) -> Any:
        url = self._config.base_url + path
        query = {k: v for k, v in (params or {}).items() if v is not None}
        request_timeout = self._config.timeout if timeout is NOT_GIVEN else timeout
        attempt = 0
        while True:
            try:
                response = await self._http.request(
                    method,
                    url,
                    json=json,
                    params=query or None,
                    headers=self._config.headers,
                    timeout=request_timeout,
                )
            except httpx.TimeoutException as exc:
                never_sent = isinstance(exc, httpx.ConnectTimeout)
                if attempt < self._config.max_retries and (never_sent or idempotent):
                    await _sleep(retry_delay(attempt, None))
                    attempt += 1
                    continue
                raise APITimeoutError(f"request timed out: {exc}", request=exc.request) from exc
            except httpx.TransportError as exc:
                never_sent = isinstance(exc, httpx.ConnectError)
                if attempt < self._config.max_retries and (never_sent or idempotent):
                    await _sleep(retry_delay(attempt, None))
                    attempt += 1
                    continue
                raise APIConnectionError(f"connection error: {exc}", request=exc.request) from exc

            if response.is_success:
                return decode_json(response)
            retry_after = parse_retry_after(response.headers)
            if attempt < self._config.max_retries and should_retry(
                response.status_code, idempotent
            ):
                await response.aclose()
                await _sleep(retry_delay(attempt, retry_after))
                attempt += 1
                continue
            raise error_from_response(response, retry_after)

    # ------------------------------------------------------------------
    # Single-request endpoints
    # ------------------------------------------------------------------

    async def health(self) -> HealthResponse:
        """``GET /health``."""
        return parse_model(HealthResponse, await self._request("GET", "/health"))

    async def scrape(
        self,
        url: Union[str, Body],
        *,
        request_timeout: Any = NOT_GIVEN,
        **options: Any,
    ) -> ScrapeResponse:
        """Scrape one page (``POST /scrape``).

        Args:
            url: the page URL, or a full ``ScrapeRequest`` model / dict.
            **options: any ``ScrapeRequest`` field (``formats``,
                ``only_main_content``, ``render_js``, ``extract``, ``ai``, ...).
        """
        base, fields = split_first(url, "url")
        body = build_body(base, options, **fields)
        data = await self._request("POST", "/scrape", json=body, timeout=request_timeout)
        return parse_model(ScrapeResponse, data)

    async def map(
        self,
        url: Union[str, Body],
        *,
        request_timeout: Any = NOT_GIVEN,
        **options: Any,
    ) -> MapResponse:
        """Discover a site's URLs (``POST /map``). ``options``: ``MapRequest`` fields."""
        base, fields = split_first(url, "url")
        body = build_body(base, options, **fields)
        data = await self._request("POST", "/map", json=body, timeout=request_timeout)
        return parse_model(MapResponse, data)

    async def search(
        self,
        url: Union[str, Body],
        q: Optional[str] = None,
        *,
        request_timeout: Any = NOT_GIVEN,
        **options: Any,
    ) -> Any:
        """Search the web content of a site (``POST /search``).

        ``options``: ``SearchRequest`` fields (``limit``, ``offset``,
        ``filter``, ``sort``). Returns the decoded JSON response.
        """
        base, fields = split_first(url, "url")
        body = build_body(base, options, q=q, **fields)
        return await self._request("POST", "/search", json=body, timeout=request_timeout)

    # ------------------------------------------------------------------
    # Jobs: creation
    # ------------------------------------------------------------------

    async def crawl(
        self,
        start_urls: Union[str, list[str], Body],
        *,
        request_timeout: Any = NOT_GIVEN,
        **options: Any,
    ) -> CreateCrawlResponse:
        """Start a distributed crawl (``POST /crawl``); returns immediately.

        Args:
            start_urls: one URL, a list of URLs, or a full ``CrawlConfig``
                model / dict.
            **options: any ``CrawlConfig`` field (``index_uid``,
                ``max_pages``, ``max_depth``, ``allowed_domains``,
                ``features``, ``webhooks``, ...).
        """
        base, fields = split_first(start_urls, "start_urls", as_list=True)
        body = build_body(base, options, **fields)
        data = await self._request(
            "POST", "/crawl", json=body, timeout=request_timeout, idempotent=False
        )
        return parse_model(CreateCrawlResponse, data)

    async def crawl_sync(
        self,
        start_urls: Union[str, list[str], Body],
        *,
        include_results: bool = False,
        results_limit: Optional[int] = None,
        request_timeout: Any = CRAWL_SYNC_TIMEOUT,
        **options: Any,
    ) -> CrawlSyncResponse:
        """Run a crawl and block until it ends (``POST /crawl/sync``).

        With ``include_results=True`` the response carries the first page of
        results (``results``). Prefer :meth:`crawl_and_wait` for long crawls:
        it polls instead of holding one HTTP request open.
        """
        base, fields = split_first(start_urls, "start_urls", as_list=True)
        body = build_body(base, options, **fields)
        params = {
            "include_results": "true" if include_results else None,
            "results_limit": results_limit,
        }
        data = await self._request(
            "POST",
            "/crawl/sync",
            json=body,
            params=params,
            timeout=request_timeout,
            idempotent=False,
        )
        return parse_model(CrawlSyncResponse, data)

    async def batch_scrape(
        self,
        urls: Union[list[str], Body],
        *,
        request_timeout: Any = NOT_GIVEN,
        **options: Any,
    ) -> BatchScrapeResponse:
        """Scrape many URLs as one job (``POST /batch/scrape``); returns immediately.

        ``options``: ``concurrency``, ``webhooks`` and any ``/scrape`` option,
        applied to every URL.
        """
        base, fields = split_first(urls, "urls", as_list=True)
        body = build_body(base, options, **fields)
        data = await self._request(
            "POST", "/batch/scrape", json=body, timeout=request_timeout, idempotent=False
        )
        return parse_model(BatchScrapeResponse, data)

    async def extract(
        self,
        urls: Union[list[str], Body],
        *,
        prompt: Optional[str] = None,
        schema: Any = None,
        request_timeout: Any = NOT_GIVEN,
        **options: Any,
    ) -> CreateExtractResponse:
        """Start a structured extraction over pages (``POST /extract``).

        Args:
            urls: URLs (globs like ``https://example.com/blog/*`` allowed), or
                a full ``ExtractRequest`` model / dict.
            prompt: what to extract, in natural language.
            schema: expected output, as a JSON Schema dict or a list of field
                definitions.
        """
        base, fields = split_first(urls, "urls", as_list=True)
        body = build_body(base, options, prompt=prompt, schema=schema, **fields)
        data = await self._request(
            "POST", "/extract", json=body, timeout=request_timeout, idempotent=False
        )
        return parse_model(CreateExtractResponse, data)

    async def get_extract(self, job_id: str) -> ExtractStatusResponse:
        """``GET /extract/{id}``: status, sources and (once completed) ``data``."""
        data = await self._request("GET", extract_path(job_id))
        return parse_model(ExtractStatusResponse, data)

    # ------------------------------------------------------------------
    # Jobs: status and control
    # ------------------------------------------------------------------

    async def get_job(self, job_id: str) -> JobStatusResponse:
        """``GET /job/{id}/status``."""
        return parse_model(
            JobStatusResponse, await self._request("GET", job_path(job_id, "/status"))
        )

    async def list_jobs(
        self,
        *,
        limit: Optional[int] = None,
        offset: Optional[int] = None,
        status: Optional[str] = None,
    ) -> list[JobStatusResponse]:
        """``GET /jobs``: the account's jobs, newest first (``limit`` at most
        200), optionally of one ``status``. Items omit ``config`` (see
        :meth:`get_job`)."""
        params = {"limit": limit, "offset": offset, "status": status}
        data = await self._request("GET", "/jobs", params=params)
        return [parse_model(JobStatusResponse, item) for item in data or []]

    async def cancel_job(self, job_id: str) -> JobStatusResponse:
        """``DELETE /job/{id}``: cancel a running or paused job."""
        data = await self._request("DELETE", job_path(job_id), idempotent=False)
        return parse_model(JobStatusResponse, data)

    async def delete_job(self, job_id: str) -> None:
        """``DELETE /job/{id}?purge=true``: delete a finished (completed,
        failed or cancelled) job and its stored results. A job that is not
        finished is a 409: cancel it first."""
        await self._request("DELETE", job_path(job_id), params={"purge": "true"}, idempotent=False)

    async def pause_job(self, job_id: str) -> JobStatusResponse:
        """``POST /job/{id}/pause``."""
        data = await self._request("POST", job_path(job_id, "/pause"), idempotent=False)
        return parse_model(JobStatusResponse, data)

    async def resume_job(self, job_id: str) -> JobStatusResponse:
        """``POST /job/{id}/resume``."""
        data = await self._request("POST", job_path(job_id, "/resume"), idempotent=False)
        return parse_model(JobStatusResponse, data)

    # ------------------------------------------------------------------
    # Jobs: results
    # ------------------------------------------------------------------

    async def job_results(
        self, job_id: str, *, limit: Optional[int] = None, cursor: Optional[str] = None
    ) -> JobResultsResponse:
        """One page of ``GET /job/{id}/results`` (``limit`` <= 100).

        ``next`` is the cursor of the following page; it is ``None`` only once
        the job is terminal and every result was returned.
        """
        data = await self._request(
            "GET", job_path(job_id, "/results"), params={"limit": limit, "cursor": cursor}
        )
        return parse_model(JobResultsResponse, data)

    async def iter_job_results(
        self,
        job_id: str,
        *,
        page_size: int = DEFAULT_RESULTS_PAGE_SIZE,
        wait: bool = False,
        poll_interval: float = DEFAULT_POLL_INTERVAL,
    ) -> AsyncIterator[JobResultItem]:
        """Iterate over every result of a job, following the cursor.

        For a finished job this yields all results then stops. For a running
        job it stops once it has caught up with the results available so
        far, unless ``wait=True``: it then keeps polling (every
        ``poll_interval`` seconds) and stops when the job is terminal and
        every result was yielded.
        """
        cursor: Optional[str] = None
        while True:
            page = await self.job_results(job_id, limit=page_size, cursor=cursor)
            for item in page.data:
                yield item
            if page.next is None:
                return
            if not page.data:
                if not wait:
                    return
                await _sleep(poll_interval)
            cursor = page.next

    # ------------------------------------------------------------------
    # Job watcher
    # ------------------------------------------------------------------

    async def iter_job_status(
        self,
        job_id: str,
        *,
        poll_interval: float = DEFAULT_POLL_INTERVAL,
        timeout: Optional[float] = None,
    ) -> AsyncIterator[JobStatusResponse]:
        """Poll ``GET /job/{id}/status`` and yield each status, ending with
        the terminal one (``completed``, ``failed`` or ``cancelled``).

        Raises:
            JobTimeoutError: the job is still running after ``timeout`` seconds.
        """
        deadline = None if timeout is None else time.monotonic() + timeout
        last: Optional[JobStatusResponse] = None
        while True:
            last = await self.get_job(job_id)
            yield last
            if is_terminal(last.status):
                return
            if deadline is not None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise JobTimeoutError(job_id, timeout or 0.0, last)
                await _sleep(min(poll_interval, remaining))
            else:
                await _sleep(poll_interval)

    async def wait_for_job(
        self,
        job_id: str,
        *,
        poll_interval: float = DEFAULT_POLL_INTERVAL,
        timeout: Optional[float] = None,
        on_progress: Optional[ProgressCallback] = None,
    ) -> JobStatusResponse:
        """Block until a job is terminal and return its final status.

        ``on_progress`` is called with every status polled (including the
        final one). A failed or cancelled job is returned, not raised: check
        ``status``.

        Raises:
            JobTimeoutError: the job is still running after ``timeout`` seconds.
        """
        final: Optional[JobStatusResponse] = None
        async for status in self.iter_job_status(
            job_id, poll_interval=poll_interval, timeout=timeout
        ):
            if on_progress is not None:
                maybe = on_progress(status)
                if maybe is not None:
                    await maybe
            final = status
        assert final is not None
        return final

    async def _collect(
        self,
        job_id: str,
        poll_interval: float,
        timeout: Optional[float],
        on_progress: Optional[ProgressCallback],
        page_size: int,
    ) -> JobResult:
        job = await self.wait_for_job(
            job_id, poll_interval=poll_interval, timeout=timeout, on_progress=on_progress
        )
        documents = [item async for item in self.iter_job_results(job_id, page_size=page_size)]
        return JobResult(job=job, documents=documents)

    async def crawl_and_wait(
        self,
        start_urls: Union[str, list[str], Body],
        *,
        poll_interval: float = DEFAULT_POLL_INTERVAL,
        timeout: Optional[float] = None,
        on_progress: Optional[ProgressCallback] = None,
        page_size: int = DEFAULT_RESULTS_PAGE_SIZE,
        **options: Any,
    ) -> JobResult:
        """Start a crawl, wait for it to end, and return its status and documents.

        Crawled pages are indexed into Meilisearch asynchronously: the last
        few documents of a crawl that just finished can take a moment to be
        readable. Re-read with :meth:`iter_job_results` if ``documents`` looks
        short of ``job.documents_sent``.
        """
        created = await self.crawl(start_urls, **options)
        return await self._collect(created.job_id, poll_interval, timeout, on_progress, page_size)

    async def batch_scrape_and_wait(
        self,
        urls: Union[list[str], Body],
        *,
        poll_interval: float = DEFAULT_POLL_INTERVAL,
        timeout: Optional[float] = None,
        on_progress: Optional[ProgressCallback] = None,
        page_size: int = DEFAULT_RESULTS_PAGE_SIZE,
        **options: Any,
    ) -> JobResult:
        """Start a batch scrape, wait for it to end, and return its status and
        one document per URL (in completion order; ``index`` is the URL's
        position in ``urls``)."""
        created = await self.batch_scrape(urls, **options)
        return await self._collect(created.job_id, poll_interval, timeout, on_progress, page_size)

    async def extract_and_wait(
        self,
        urls: Union[list[str], Body],
        *,
        prompt: Optional[str] = None,
        schema: Any = None,
        poll_interval: float = DEFAULT_POLL_INTERVAL,
        timeout: Optional[float] = None,
        on_progress: Optional[ProgressCallback] = None,
        page_size: int = DEFAULT_RESULTS_PAGE_SIZE,
        **options: Any,
    ) -> ExtractJobResult:
        """Start an extraction, wait for it to end, and return the extraction
        (``result.data``), its status and the pages it used (``documents``)."""
        created = await self.extract(urls, prompt=prompt, schema=schema, **options)
        collected = await self._collect(
            created.job_id, poll_interval, timeout, on_progress, page_size
        )
        extract = await self.get_extract(created.job_id)
        return ExtractJobResult(job=collected.job, documents=collected.documents, extract=extract)
