"""Transport-independent pieces shared by the sync and async clients."""

from __future__ import annotations

import email.utils
import logging
import os
import random
import time
from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Optional, TypeVar, Union
from urllib.parse import quote

import httpx
from pydantic import BaseModel, ValidationError

from ._generated.models import ExtractStatusResponse, JobResultItem, JobStatusResponse
from ._version import __version__

logger = logging.getLogger("scrapix")

DEFAULT_BASE_URL = "https://scrapix.meilisearch.dev"
DEFAULT_TIMEOUT = 60.0
DEFAULT_MAX_RETRIES = 2
#: `POST /crawl/sync` blocks until the crawl ends (server-side limit: 1 hour).
CRAWL_SYNC_TIMEOUT = 3600.0
DEFAULT_POLL_INTERVAL = 2.0
#: Page size used when iterating over job results (the API maximum).
DEFAULT_RESULTS_PAGE_SIZE = 100

#: Job statuses after which a job never changes again.
TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled"})

#: Statuses retried for requests that are safe to repeat.
RETRYABLE_STATUSES = frozenset({408, 429, 500, 502, 503, 504})
#: Longest delay honored from a `Retry-After` header (seconds).
MAX_RETRY_AFTER = 60.0
_BACKOFF_BASE = 0.5
_BACKOFF_MAX = 8.0

M = TypeVar("M", bound=BaseModel)


class _NotGiven:
    """Sentinel: "use the client default" (``None`` means "no timeout")."""

    def __repr__(self) -> str:
        return "NOT_GIVEN"


NOT_GIVEN: Any = _NotGiven()

Body = Union[BaseModel, Mapping[str, Any]]


@dataclass(frozen=True)
class ClientConfig:
    base_url: str
    headers: dict[str, str]
    timeout: Optional[float]
    max_retries: int


def resolve_config(
    api_key: Optional[str],
    base_url: Optional[str],
    timeout: Optional[float],
    max_retries: int,
    headers: Optional[Mapping[str, str]],
) -> ClientConfig:
    key = api_key if api_key is not None else os.environ.get("SCRAPIX_API_KEY")
    url = base_url or os.environ.get("SCRAPIX_API_URL") or DEFAULT_BASE_URL
    merged = {
        "Accept": "application/json",
        "User-Agent": f"scrapix-python/{__version__}",
    }
    if key:
        merged.update(auth_headers(key))
    if headers:
        merged.update(headers)
    return ClientConfig(
        base_url=url.rstrip("/"),
        headers=merged,
        timeout=timeout,
        max_retries=max(0, max_retries),
    )


def auth_headers(key: str) -> dict[str, str]:
    """Credentials header for `key`.

    Scrapix API keys (``sk_live_...`` / ``sk_test_...``) go in ``X-API-Key``;
    anything else is treated as an OAuth access token and sent as
    ``Authorization: Bearer``. Both backends read Bearer tokens as OAuth
    tokens only, so an API key must not be sent as Bearer.
    """
    if key.startswith("sk_"):
        return {"X-API-Key": key}
    return {"Authorization": f"Bearer {key}"}


def job_path(job_id: str, suffix: str = "") -> str:
    return f"/job/{quote(job_id, safe='')}{suffix}"


def extract_path(job_id: str) -> str:
    return f"/extract/{quote(job_id, safe='')}"


def is_terminal(status: Any) -> bool:
    value = status.value if isinstance(status, Enum) else status
    return value in TERMINAL_STATUSES


# ---------------------------------------------------------------------------
# Request bodies
# ---------------------------------------------------------------------------


def to_jsonable(value: Any) -> Any:
    """Convert pydantic models / enums nested anywhere in `value` to JSON types."""
    if isinstance(value, BaseModel):
        return value.model_dump(mode="json", by_alias=True, exclude_none=True)
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Mapping):
        return {k: to_jsonable(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [to_jsonable(v) for v in value]
    return value


def build_body(base: Optional[Body], options: Mapping[str, Any], **fields: Any) -> dict[str, Any]:
    """Merge a request model/dict, explicit fields and keyword options.

    Later sources win; ``None`` values are dropped so the API applies its
    own defaults.
    """
    body: dict[str, Any] = {}
    if base is not None:
        converted = to_jsonable(base)
        if not isinstance(converted, dict):
            raise TypeError("request body must be a mapping or a pydantic model")
        body.update(converted)
    for source in (fields, options):
        for key, value in source.items():
            if value is not None:
                body[key] = to_jsonable(value)
    return body


def split_first(
    first: Union[str, list[str], Body, None], key: str, *, as_list: bool = False
) -> tuple[Optional[Body], dict[str, Any]]:
    """Interpret a method's first argument: a plain value for `key`, or a
    full request model / mapping."""
    if first is None:
        return None, {}
    if isinstance(first, str):
        return None, {key: [first] if as_list else first}
    if isinstance(first, list):
        return None, {key: first}
    return first, {}


# ---------------------------------------------------------------------------
# Responses
# ---------------------------------------------------------------------------


def parse_model(model: type[M], data: Any) -> M:
    """Validate `data` as `model`, leniently.

    The API may add fields (kept, models allow extras) or send shapes the
    spec does not describe exactly; in that case the data is still returned
    (unvalidated) instead of failing the call.
    """
    try:
        return model.model_validate(data)
    except ValidationError as exc:
        if not isinstance(data, Mapping):
            raise
        logger.warning(
            "scrapix: response did not match %s (%d validation errors); returning it unvalidated",
            model.__name__,
            exc.error_count(),
        )
        return model.model_construct(**data)


def decode_json(response: httpx.Response) -> Any:
    if response.status_code == 204 or not response.content:
        return None
    try:
        return response.json()
    except ValueError:
        return response.text


# ---------------------------------------------------------------------------
# Retries
# ---------------------------------------------------------------------------


def parse_retry_after(headers: httpx.Headers) -> Optional[float]:
    """Seconds requested by a `Retry-After` header (delta-seconds or HTTP date)."""
    value = headers.get("retry-after")
    if not value:
        return None
    try:
        return max(0.0, float(value))
    except ValueError:
        pass
    try:
        parsed = email.utils.parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return None
    if parsed is None:
        return None
    return max(0.0, float(parsed.timestamp()) - time.time())


def should_retry(status: int, retry_server_errors: bool) -> bool:
    """429 is always retried (the request was rejected before any work);
    408/5xx only for requests that are safe to repeat."""
    if status == 429:
        return True
    return retry_server_errors and status in RETRYABLE_STATUSES


def retry_delay(attempt: int, retry_after: Optional[float]) -> float:
    """Delay before retry number `attempt` (0-based)."""
    if retry_after is not None:
        return min(retry_after, MAX_RETRY_AFTER)
    backoff = min(_BACKOFF_BASE * (2**attempt), _BACKOFF_MAX)
    return float(backoff * random.uniform(0.75, 1.25))


# ---------------------------------------------------------------------------
# Results of the *_and_wait helpers
# ---------------------------------------------------------------------------


@dataclass
class JobResult:
    """Outcome of a job run with a ``*_and_wait`` helper.

    Attributes:
        job: the final job status (``job.status`` is ``completed``,
            ``failed`` or ``cancelled``).
        documents: every result the job produced, in order, each shaped like
            a ``/scrape`` response (failed pages have ``success=False`` and
            an ``error``).
    """

    job: JobStatusResponse
    documents: list[JobResultItem] = field(default_factory=list)

    @property
    def job_id(self) -> str:
        return self.job.job_id

    @property
    def status(self) -> str:
        status = self.job.status
        return str(status.value if isinstance(status, Enum) else status)

    @property
    def succeeded(self) -> bool:
        return self.status == "completed"


@dataclass
class ExtractJobResult(JobResult):
    """Outcome of :meth:`Scrapix.extract_and_wait`: ``extract`` is the
    ``GET /extract/{id}`` response (``extract.data`` holds the extraction)."""

    extract: Optional[ExtractStatusResponse] = None

    @property
    def data(self) -> Any:
        return self.extract.data if self.extract is not None else None
