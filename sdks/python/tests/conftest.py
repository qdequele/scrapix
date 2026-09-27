from __future__ import annotations

import json
from collections import defaultdict, deque
from typing import Any, Callable, Union

import httpx
import pytest

import scrapix._async_client
import scrapix._client

BASE = "https://api.test"

Reply = Union[httpx.Response, Callable[[httpx.Request], httpx.Response], Exception]


class MockAPI:
    """A tiny scripted HTTP server for httpx.MockTransport.

    ``add(method, path, *replies)`` queues replies for a route; the last reply
    of a route is repeated once the queue is drained.
    """

    def __init__(self) -> None:
        self.routes: dict[tuple[str, str], deque[Reply]] = defaultdict(deque)
        self.requests: list[httpx.Request] = []

    def add(self, method: str, path: str, *replies: Reply) -> MockAPI:
        self.routes[(method, path)].extend(replies)
        return self

    def handler(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        queue = self.routes.get((request.method, request.url.path))
        if not queue:
            return httpx.Response(404, json={"error": "no route", "code": "not_found"})
        reply = queue.popleft() if len(queue) > 1 else queue[0]
        if isinstance(reply, Exception):
            raise reply
        if callable(reply):
            return reply(request)
        return reply

    def calls(self, method: str, path: str) -> list[httpx.Request]:
        return [r for r in self.requests if r.method == method and r.url.path == path]

    def sync_client(self, **kwargs: Any) -> scrapix.Scrapix:
        kwargs.setdefault("api_key", "sk_test_123")
        kwargs.setdefault("base_url", BASE)
        return scrapix.Scrapix(
            http_client=httpx.Client(transport=httpx.MockTransport(self.handler)), **kwargs
        )

    def async_client(self, **kwargs: Any) -> scrapix.AsyncScrapix:
        kwargs.setdefault("api_key", "sk_test_123")
        kwargs.setdefault("base_url", BASE)
        return scrapix.AsyncScrapix(
            http_client=httpx.AsyncClient(transport=httpx.MockTransport(self.handler)), **kwargs
        )


def body(request: httpx.Request) -> Any:
    return json.loads(request.content)


def job_status(job_id: str = "job-1", status: str = "running", **extra: Any) -> dict[str, Any]:
    return {
        "job_id": job_id,
        "job_type": extra.pop("job_type", "crawl"),
        "status": status,
        "index_uid": "idx",
        "pages_crawled": extra.pop("pages_crawled", 0),
        "pages_indexed": 0,
        "documents_sent": 0,
        "errors": 0,
        "crawl_rate": 0.0,
        **extra,
    }


def results_page(
    items: list[str],
    next_cursor: Union[str, None],
    status: str = "completed",
    job_id: str = "job-1",
    job_type: str = "crawl",
) -> dict[str, Any]:
    return {
        "job_id": job_id,
        "job_type": job_type,
        "status": status,
        "total": len(items),
        "next": next_cursor,
        "data": [{"success": True, "url": url} for url in items],
    }


@pytest.fixture
def api() -> MockAPI:
    return MockAPI()


@pytest.fixture(autouse=True)
def sleeps(monkeypatch: pytest.MonkeyPatch) -> list[float]:
    """Record sleeps instead of sleeping (sync and async clients)."""
    recorded: list[float] = []

    def fake_sleep(seconds: float) -> None:
        recorded.append(seconds)

    async def fake_async_sleep(seconds: float) -> None:
        recorded.append(seconds)

    monkeypatch.setattr(scrapix._client, "_sleep", fake_sleep)
    monkeypatch.setattr(scrapix._async_client, "_sleep", fake_async_sleep)
    return recorded


@pytest.fixture
def anyio_backend() -> str:
    return "asyncio"


@pytest.fixture(autouse=True)
def clean_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("SCRAPIX_API_KEY", raising=False)
    monkeypatch.delenv("SCRAPIX_API_URL", raising=False)
