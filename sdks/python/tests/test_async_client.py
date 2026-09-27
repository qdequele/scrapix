from __future__ import annotations

import httpx
import pytest
from conftest import MockAPI, body, job_status, results_page

import scrapix
from scrapix import AsyncScrapix, JobStatusResponse, JobTimeoutError, NotFoundError

pytestmark = pytest.mark.anyio

SCRAPE_OK = {
    "success": True,
    "url": "https://example.com",
    "status_code": 200,
    "scrape_duration_ms": 1,
}


async def test_scrape_sends_auth_and_body(api: MockAPI) -> None:
    api.add("POST", "/scrape", httpx.Response(200, json={**SCRAPE_OK, "markdown": "md"}))
    async with api.async_client(api_key="sk_live_k") as client:
        page = await client.scrape("https://example.com", formats=["markdown"])
    assert page.markdown == "md"
    assert api.requests[0].headers["x-api-key"] == "sk_live_k"
    assert body(api.requests[0]) == {"url": "https://example.com", "formats": ["markdown"]}


async def test_error_mapping(api: MockAPI) -> None:
    api.add(
        "GET",
        "/job/nope/status",
        httpx.Response(404, json={"error": "Job not found", "code": "not_found"}),
    )
    client = api.async_client()
    with pytest.raises(NotFoundError) as info:
        await client.get_job("nope")
    assert info.value.code == "not_found"
    assert info.value.status_code == 404


async def test_retries_429(api: MockAPI, sleeps: list[float]) -> None:
    api.add(
        "POST",
        "/batch/scrape",
        httpx.Response(429, headers={"retry-after": "2"}, json={"error": "slow", "code": "x"}),
        httpx.Response(
            200, json={"job_id": "b", "status": "running", "urls_count": 1, "message": "ok"}
        ),
    )
    created = await api.async_client().batch_scrape(["https://a.test"])
    assert created.job_id == "b"
    assert sleeps == [2.0]


async def test_no_retry_on_5xx_for_job_control(api: MockAPI) -> None:
    api.add("POST", "/job/j/pause", httpx.Response(500, json={"error": "x", "code": "internal"}))
    with pytest.raises(scrapix.InternalServerError):
        await api.async_client().pause_job("j")
    assert len(api.requests) == 1


async def test_iter_job_results(api: MockAPI) -> None:
    api.add(
        "GET",
        "/job/job-1/results",
        httpx.Response(200, json=results_page(["a"], "c1")),
        httpx.Response(200, json=results_page(["b"], None)),
    )
    client = api.async_client()
    urls = [item.url async for item in client.iter_job_results("job-1")]
    assert urls == ["a", "b"]


async def test_wait_for_job_with_async_callback(api: MockAPI, sleeps: list[float]) -> None:
    api.add(
        "GET",
        "/job/job-1/status",
        httpx.Response(200, json=job_status(status="running")),
        httpx.Response(200, json=job_status(status="completed")),
    )
    seen: list[str] = []

    async def on_progress(status: JobStatusResponse) -> None:
        seen.append(status.status.value)

    final = await api.async_client().wait_for_job("job-1", poll_interval=3, on_progress=on_progress)
    assert final.status == "completed"
    assert seen == ["running", "completed"]
    assert sleeps == [3]


async def test_wait_for_job_timeout(api: MockAPI) -> None:
    api.add("GET", "/job/job-1/status", httpx.Response(200, json=job_status(status="pending")))
    with pytest.raises(JobTimeoutError):
        await api.async_client().wait_for_job("job-1", timeout=0)


async def test_crawl_and_wait(api: MockAPI) -> None:
    api.add(
        "POST",
        "/crawl",
        httpx.Response(
            200,
            json={
                "job_id": "job-1",
                "status": "pending",
                "index_uid": "i",
                "start_urls_count": 2,
                "message": "ok",
            },
        ),
    )
    api.add("GET", "/job/job-1/status", httpx.Response(200, json=job_status(status="completed")))
    api.add("GET", "/job/job-1/results", httpx.Response(200, json=results_page(["a", "b"], None)))
    result = await api.async_client().crawl_and_wait(["https://a.test", "https://b.test"])
    assert result.succeeded
    assert [d.url for d in result.documents] == ["a", "b"]


async def test_default_client_is_closed_by_context_manager() -> None:
    async with AsyncScrapix(base_url="https://api.test") as client:
        http = client._http
    assert http.is_closed
