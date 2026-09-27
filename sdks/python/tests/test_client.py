from __future__ import annotations

import email.utils
import logging
import time

import httpx
import pytest
from conftest import BASE, MockAPI, body, job_status, results_page

import scrapix
from scrapix import (
    AuthenticationError,
    BadRequestError,
    ConflictError,
    InsufficientCreditsError,
    InternalServerError,
    JobTimeoutError,
    NotFoundError,
    RateLimitError,
    ScrapeRequest,
    Scrapix,
)
from scrapix.models import AiExtractOptions, AiOptions

SCRAPE_OK = {
    "success": True,
    "url": "https://example.com",
    "status_code": 200,
    "scrape_duration_ms": 12,
}

# ---------------------------------------------------------------------------
# Configuration & auth
# ---------------------------------------------------------------------------


def test_api_key_is_sent_as_x_api_key(api: MockAPI) -> None:
    api.add("POST", "/scrape", httpx.Response(200, json=SCRAPE_OK))
    api.sync_client(api_key="sk_live_abc").scrape("https://example.com")
    request = api.requests[0]
    assert request.headers["x-api-key"] == "sk_live_abc"
    assert "authorization" not in request.headers
    assert request.headers["user-agent"] == f"scrapix-python/{scrapix.__version__}"


def test_oauth_token_is_sent_as_bearer(api: MockAPI) -> None:
    api.add("POST", "/scrape", httpx.Response(200, json=SCRAPE_OK))
    api.sync_client(api_key="oauth-access-token").scrape("https://example.com")
    request = api.requests[0]
    assert request.headers["authorization"] == "Bearer oauth-access-token"
    assert "x-api-key" not in request.headers


def test_env_vars_configure_key_and_base_url(api: MockAPI, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SCRAPIX_API_KEY", "sk_test_env")
    monkeypatch.setenv("SCRAPIX_API_URL", "https://self-hosted.test/")
    api.add(
        "GET",
        "/health",
        httpx.Response(200, json={"status": "ok", "version": "1", "kafka_connected": True}),
    )
    client = Scrapix(http_client=httpx.Client(transport=httpx.MockTransport(api.handler)))
    assert client.base_url == "https://self-hosted.test"
    client.health()
    assert str(api.requests[0].url) == "https://self-hosted.test/health"
    assert api.requests[0].headers["x-api-key"] == "sk_test_env"


def test_default_base_url_and_no_key() -> None:
    client = Scrapix()
    assert client.base_url == "https://scrapix.meilisearch.dev"
    assert "X-API-Key" not in client._config.headers
    assert "Authorization" not in client._config.headers
    client.close()


# ---------------------------------------------------------------------------
# Request bodies & responses
# ---------------------------------------------------------------------------


def test_scrape_builds_body_from_kwargs_and_parses_response(api: MockAPI) -> None:
    api.add(
        "POST",
        "/scrape",
        httpx.Response(200, json={**SCRAPE_OK, "markdown": "# Hi", "brand_new_field": 1}),
    )
    page = api.sync_client().scrape(
        "https://example.com",
        formats=["markdown", "metadata"],
        only_main_content=None,  # dropped
        ai=AiOptions(summary=True, extract=AiExtractOptions(prompt="authors")),
    )
    assert body(api.requests[0]) == {
        "url": "https://example.com",
        "formats": ["markdown", "metadata"],
        "ai": {"summary": True, "extract": {"prompt": "authors"}},
    }
    assert isinstance(page, scrapix.ScrapeResponse)
    assert page.markdown == "# Hi"
    # unknown fields are kept
    assert page.model_extra == {"brand_new_field": 1}


def test_scrape_accepts_a_request_model(api: MockAPI) -> None:
    api.add("POST", "/scrape", httpx.Response(200, json=SCRAPE_OK))
    request = ScrapeRequest(url="https://example.com", render_js=True)
    api.sync_client().scrape(request, timeout_ms=5000)
    assert body(api.requests[0]) == {
        "url": "https://example.com",
        "render_js": True,
        "timeout_ms": 5000,
    }


def test_crawl_wraps_a_single_url(api: MockAPI) -> None:
    api.add(
        "POST",
        "/crawl",
        httpx.Response(
            200,
            json={
                "job_id": "job-1",
                "status": "pending",
                "index_uid": "docs",
                "start_urls_count": 1,
                "message": "ok",
            },
        ),
    )
    created = api.sync_client().crawl("https://docs.example.com", index_uid="docs", max_pages=10)
    assert created.job_id == "job-1"
    assert body(api.requests[0]) == {
        "start_urls": ["https://docs.example.com"],
        "index_uid": "docs",
        "max_pages": 10,
    }


def test_crawl_sync_query_params(api: MockAPI) -> None:
    api.add(
        "POST",
        "/crawl/sync",
        httpx.Response(
            200,
            json={**job_status(status="completed"), "results": results_page(["a"], None)},
        ),
    )
    response = api.sync_client().crawl_sync(
        ["https://a.test"], include_results=True, results_limit=5
    )
    request = api.requests[0]
    assert request.url.params["include_results"] == "true"
    assert request.url.params["results_limit"] == "5"
    assert response.results is not None
    assert response.results.data[0].url == "a"


def test_extract_body(api: MockAPI) -> None:
    api.add("POST", "/extract", httpx.Response(200, json={"job_id": "ex-1", "status": "running"}))
    api.sync_client().extract(
        ["https://example.com/blog/*"], prompt="titles", schema={"type": "object"}
    )
    assert body(api.requests[0]) == {
        "urls": ["https://example.com/blog/*"],
        "prompt": "titles",
        "schema": {"type": "object"},
    }


def test_list_jobs_and_job_control(api: MockAPI) -> None:
    api.add("GET", "/jobs", httpx.Response(200, json=[job_status("a"), job_status("b")]))
    api.add("POST", "/job/a/pause", httpx.Response(200, json=job_status("a", "paused")))
    api.add("POST", "/job/a/resume", httpx.Response(200, json=job_status("a", "running")))
    api.add("DELETE", "/job/a", httpx.Response(200, json=job_status("a", "cancelled")))
    client = api.sync_client()
    jobs = client.list_jobs(limit=2)
    assert [j.job_id for j in jobs] == ["a", "b"]
    assert api.requests[0].url.params["limit"] == "2"
    assert "offset" not in api.requests[0].url.params
    assert client.pause_job("a").status == "paused"
    assert client.resume_job("a").status == "running"
    assert client.cancel_job("a").status == "cancelled"


def test_job_ids_are_path_escaped(api: MockAPI) -> None:
    # httpx decodes `url.path`; the raw path keeps the escaping.
    api.add("GET", "/job/a/b/status", httpx.Response(200, json=job_status("a/b")))
    api.sync_client().get_job("a/b")
    assert api.requests[0].url.raw_path == b"/job/a%2Fb/status"


def test_lenient_parsing_of_unexpected_shapes(
    api: MockAPI, caplog: pytest.LogCaptureFixture
) -> None:
    api.add("GET", "/job/j/status", httpx.Response(200, json={"job_id": "j", "status": "running"}))
    with caplog.at_level(logging.WARNING, logger="scrapix"):
        job = api.sync_client().get_job("j")
    assert job.job_id == "j"
    assert "did not match JobStatusResponse" in caplog.text


# ---------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("status", "code", "error_type"),
    [
        (400, "validation_error", BadRequestError),
        (401, "invalid_api_key", AuthenticationError),
        (402, "insufficient_credits", InsufficientCreditsError),
        (404, "not_found", NotFoundError),
        (409, "conflict", ConflictError),
        (500, "internal", InternalServerError),
    ],
)
def test_error_mapping(api: MockAPI, status: int, code: str, error_type: type) -> None:
    api.add(
        "GET",
        "/job/x/status",
        httpx.Response(status, json={"error": "Nope", "code": code, "details": {"f": 1}}),
    )
    with pytest.raises(error_type) as info:
        api.sync_client(max_retries=0).get_job("x")
    err = info.value
    assert isinstance(err, scrapix.APIStatusError)
    assert err.status_code == status
    assert err.code == code
    assert err.message == "Nope"
    assert err.details == {"f": 1}
    assert str(err) == f"{status} {code}: Nope"


def test_non_json_error_body(api: MockAPI) -> None:
    api.add("POST", "/scrape", httpx.Response(502, text="<html>Bad Gateway</html>"))
    with pytest.raises(InternalServerError) as info:
        api.sync_client(max_retries=0).scrape("https://example.com")
    assert info.value.code is None
    assert info.value.message == "<html>Bad Gateway</html>"


def test_connection_error(api: MockAPI) -> None:
    api.add("GET", "/health", httpx.ConnectError("refused"))
    with pytest.raises(scrapix.APIConnectionError):
        api.sync_client(max_retries=0).health()


def test_timeout_error(api: MockAPI) -> None:
    api.add("GET", "/health", httpx.ReadTimeout("slow"))
    with pytest.raises(scrapix.APITimeoutError):
        api.sync_client(max_retries=0).health()


# ---------------------------------------------------------------------------
# Retries
# ---------------------------------------------------------------------------


def test_retries_429_honoring_retry_after(api: MockAPI, sleeps: list[float]) -> None:
    api.add(
        "POST",
        "/scrape",
        httpx.Response(
            429,
            headers={"retry-after": "3"},
            json={"error": "Rate limit exceeded", "code": "rate_limit_exceeded"},
        ),
        httpx.Response(200, json=SCRAPE_OK),
    )
    page = api.sync_client().scrape("https://example.com")
    assert page.success
    assert len(api.requests) == 2
    assert sleeps == [3.0]


def test_retry_after_http_date(api: MockAPI, sleeps: list[float]) -> None:
    when = email.utils.formatdate(time.time() + 30, usegmt=True)
    api.add(
        "GET",
        "/health",
        httpx.Response(429, headers={"retry-after": when}, json={"error": "slow down"}),
        httpx.Response(200, json={"status": "ok", "version": "1", "kafka_connected": True}),
    )
    api.sync_client().health()
    assert 25 <= sleeps[0] <= 30


def test_rate_limit_error_after_retries_exhausted(api: MockAPI, sleeps: list[float]) -> None:
    api.add(
        "POST",
        "/crawl",
        httpx.Response(
            429,
            headers={"retry-after": "1"},
            json={"error": "Rate limited", "code": "rate_limit_exceeded", "retry_after_seconds": 1},
        ),
    )
    with pytest.raises(RateLimitError) as info:
        api.sync_client(max_retries=2).crawl("https://a.test")
    assert info.value.retry_after == 1.0
    assert len(api.requests) == 3
    assert sleeps == [1.0, 1.0]


def test_retries_5xx_on_safe_requests_with_backoff(api: MockAPI, sleeps: list[float]) -> None:
    api.add(
        "GET",
        "/job/j/status",
        httpx.Response(503, json={"error": "down", "code": "service_unavailable"}),
        httpx.Response(502, text="bad gateway"),
        httpx.Response(200, json=job_status("j")),
    )
    assert api.sync_client().get_job("j").job_id == "j"
    assert len(api.requests) == 3
    assert len(sleeps) == 2
    assert 0.375 <= sleeps[0] <= 0.625
    assert 0.75 <= sleeps[1] <= 1.25


def test_job_creation_is_not_retried_on_5xx(api: MockAPI, sleeps: list[float]) -> None:
    api.add("POST", "/crawl", httpx.Response(503, json={"error": "down", "code": "x"}))
    with pytest.raises(InternalServerError):
        api.sync_client().crawl("https://a.test")
    assert len(api.requests) == 1
    assert sleeps == []


def test_connect_errors_are_retried_even_for_job_creation(api: MockAPI) -> None:
    created = {
        "job_id": "j",
        "status": "pending",
        "index_uid": "i",
        "start_urls_count": 1,
        "message": "ok",
    }
    api.add("POST", "/crawl", httpx.ConnectError("refused"), httpx.Response(200, json=created))
    assert api.sync_client().crawl("https://a.test").job_id == "j"
    assert len(api.requests) == 2


def test_max_retries_zero_disables_retries(api: MockAPI) -> None:
    api.add("GET", "/health", httpx.Response(503, json={"error": "down", "code": "x"}))
    with pytest.raises(InternalServerError):
        api.sync_client(max_retries=0).health()
    assert len(api.requests) == 1


# ---------------------------------------------------------------------------
# Pagination
# ---------------------------------------------------------------------------


def test_iter_job_results_follows_cursor(api: MockAPI) -> None:
    def page(request: httpx.Request) -> httpx.Response:
        cursor = request.url.params.get("cursor")
        pages = {
            None: results_page(["u1", "u2"], "c1"),
            "c1": results_page(["u3", "u4"], "c2"),
            "c2": results_page(["u5"], None),
        }
        return httpx.Response(200, json=pages[cursor])

    api.add("GET", "/job/job-1/results", page)
    urls = [item.url for item in api.sync_client().iter_job_results("job-1", page_size=2)]
    assert urls == ["u1", "u2", "u3", "u4", "u5"]
    assert [r.url.params.get("cursor") for r in api.requests] == [None, "c1", "c2"]
    assert all(r.url.params["limit"] == "2" for r in api.requests)


def test_iter_job_results_stops_when_caught_up_with_running_job(api: MockAPI) -> None:
    api.add(
        "GET",
        "/job/job-1/results",
        httpx.Response(200, json=results_page(["u1"], "c1", status="running")),
        httpx.Response(200, json=results_page([], "c1", status="running")),
    )
    urls = [item.url for item in api.sync_client().iter_job_results("job-1")]
    assert urls == ["u1"]
    assert len(api.requests) == 2


def test_iter_job_results_wait_polls_until_terminal(api: MockAPI, sleeps: list[float]) -> None:
    api.add(
        "GET",
        "/job/job-1/results",
        httpx.Response(200, json=results_page(["u1"], "c1", status="running")),
        httpx.Response(200, json=results_page([], "c1", status="running")),
        httpx.Response(200, json=results_page(["u2"], "c2", status="running")),
        httpx.Response(200, json=results_page([], None, status="completed")),
    )
    client = api.sync_client()
    urls = [i.url for i in client.iter_job_results("job-1", wait=True, poll_interval=1.5)]
    assert urls == ["u1", "u2"]
    assert [r.url.params.get("cursor") for r in api.requests] == [None, "c1", "c1", "c2"]
    assert sleeps == [1.5]


# ---------------------------------------------------------------------------
# Job watcher
# ---------------------------------------------------------------------------


def test_wait_for_job_polls_until_terminal(api: MockAPI, sleeps: list[float]) -> None:
    api.add(
        "GET",
        "/job/job-1/status",
        httpx.Response(200, json=job_status(status="pending")),
        httpx.Response(200, json=job_status(status="running", pages_crawled=4)),
        httpx.Response(200, json=job_status(status="paused", pages_crawled=4)),
        httpx.Response(200, json=job_status(status="completed", pages_crawled=9)),
    )
    seen: list[str] = []
    final = api.sync_client().wait_for_job(
        "job-1", poll_interval=0.5, on_progress=lambda s: seen.append(s.status.value)
    )
    assert final.status == "completed"
    assert final.pages_crawled == 9
    assert seen == ["pending", "running", "paused", "completed"]
    assert sleeps == [0.5, 0.5, 0.5]


@pytest.mark.parametrize("terminal", ["failed", "cancelled"])
def test_wait_for_job_returns_failed_jobs(api: MockAPI, terminal: str) -> None:
    api.add(
        "GET",
        "/job/job-1/status",
        httpx.Response(200, json=job_status(status=terminal, error_message="boom")),
    )
    final = api.sync_client().wait_for_job("job-1")
    assert final.status == terminal


def test_wait_for_job_timeout(api: MockAPI) -> None:
    api.add("GET", "/job/job-1/status", httpx.Response(200, json=job_status(status="running")))
    with pytest.raises(JobTimeoutError) as info:
        api.sync_client().wait_for_job("job-1", timeout=0)
    assert info.value.job_id == "job-1"
    assert info.value.last_status.status == "running"


def test_iter_job_status_yields_progress(api: MockAPI) -> None:
    api.add(
        "GET",
        "/job/job-1/status",
        httpx.Response(200, json=job_status(status="running", pages_crawled=1)),
        httpx.Response(200, json=job_status(status="completed", pages_crawled=2)),
    )
    counts = [s.pages_crawled for s in api.sync_client().iter_job_status("job-1")]
    assert counts == [1, 2]


def test_crawl_and_wait_returns_status_and_all_documents(api: MockAPI) -> None:
    api.add(
        "POST",
        "/crawl",
        httpx.Response(
            200,
            json={
                "job_id": "job-1",
                "status": "pending",
                "index_uid": "i",
                "start_urls_count": 1,
                "message": "ok",
            },
        ),
    )
    api.add(
        "GET",
        "/job/job-1/status",
        httpx.Response(200, json=job_status(status="running")),
        httpx.Response(200, json=job_status(status="completed", documents_sent=3)),
    )
    api.add(
        "GET",
        "/job/job-1/results",
        httpx.Response(200, json=results_page(["a", "b"], "c1")),
        httpx.Response(200, json=results_page(["c"], None)),
    )
    progress: list[str] = []
    result = api.sync_client().crawl_and_wait(
        "https://a.test", max_pages=3, on_progress=lambda s: progress.append(s.status.value)
    )
    assert result.succeeded
    assert result.job_id == "job-1"
    assert result.status == "completed"
    assert [d.url for d in result.documents] == ["a", "b", "c"]
    assert progress == ["running", "completed"]
    assert body(api.calls("POST", "/crawl")[0]) == {
        "start_urls": ["https://a.test"],
        "max_pages": 3,
    }


def test_batch_scrape_and_wait(api: MockAPI) -> None:
    api.add(
        "POST",
        "/batch/scrape",
        httpx.Response(
            200, json={"job_id": "b-1", "status": "running", "urls_count": 2, "message": "ok"}
        ),
    )
    api.add(
        "GET",
        "/job/b-1/status",
        httpx.Response(200, json=job_status("b-1", "completed", job_type="batch_scrape")),
    )
    api.add(
        "GET",
        "/job/b-1/results",
        httpx.Response(
            200, json=results_page(["x", "y"], None, job_id="b-1", job_type="batch_scrape")
        ),
    )
    result = api.sync_client().batch_scrape_and_wait(
        ["https://x.test", "https://y.test"], formats=["markdown"], concurrency=2
    )
    assert [d.url for d in result.documents] == ["x", "y"]
    assert body(api.calls("POST", "/batch/scrape")[0]) == {
        "urls": ["https://x.test", "https://y.test"],
        "formats": ["markdown"],
        "concurrency": 2,
    }


def test_extract_and_wait(api: MockAPI) -> None:
    api.add("POST", "/extract", httpx.Response(200, json={"job_id": "ex-1", "status": "running"}))
    api.add(
        "GET",
        "/job/ex-1/status",
        httpx.Response(200, json=job_status("ex-1", "running", job_type="extract")),
        httpx.Response(200, json=job_status("ex-1", "completed", job_type="extract")),
    )
    api.add(
        "GET",
        "/job/ex-1/results",
        httpx.Response(200, json=results_page(["p"], None, job_id="ex-1", job_type="extract")),
    )
    api.add(
        "GET",
        "/extract/ex-1",
        httpx.Response(
            200,
            json={
                "job_id": "ex-1",
                "status": "completed",
                "sources": [{"url": "p", "success": True}],
                "data": {"title": "Hello"},
            },
        ),
    )
    result = api.sync_client().extract_and_wait(["https://p.test"], prompt="title")
    assert result.data == {"title": "Hello"}
    assert result.extract is not None
    assert result.extract.sources[0].url == "p"
    assert [d.url for d in result.documents] == ["p"]


def test_context_manager_closes_owned_client() -> None:
    with Scrapix(base_url=BASE) as client:
        http = client._http
    assert http.is_closed
