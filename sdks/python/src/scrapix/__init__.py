"""Official Python SDK for the Scrapix API.

>>> from scrapix import Scrapix
>>> client = Scrapix()  # reads SCRAPIX_API_KEY / SCRAPIX_API_URL
>>> page = client.scrape("https://example.com", formats=["markdown"])
>>> result = client.crawl_and_wait("https://docs.example.com", max_pages=50)
"""

from ._async_client import AsyncScrapix
from ._base import (
    DEFAULT_BASE_URL,
    NOT_GIVEN,
    TERMINAL_STATUSES,
    ExtractJobResult,
    JobResult,
)
from ._client import Scrapix
from ._errors import (
    APIConnectionError,
    APIStatusError,
    APITimeoutError,
    AuthenticationError,
    BadRequestError,
    ConflictError,
    InsufficientCreditsError,
    InternalServerError,
    JobTimeoutError,
    NotFoundError,
    PermissionDeniedError,
    RateLimitError,
    ScrapixError,
    UnprocessableEntityError,
)
from ._generated.models import (
    BatchScrapeRequest,
    BatchScrapeResponse,
    CrawlConfig,
    CrawlSyncResponse,
    CreateCrawlResponse,
    CreateExtractResponse,
    ExtractRequest,
    ExtractStatusResponse,
    JobKind,
    JobResultItem,
    JobResultsResponse,
    JobStatus,
    JobStatusResponse,
    MapRequest,
    MapResponse,
    ScrapeFormat,
    ScrapeRequest,
    ScrapeResponse,
    SearchRequest,
)
from ._version import __version__

__all__ = [
    "__version__",
    "Scrapix",
    "AsyncScrapix",
    "DEFAULT_BASE_URL",
    "NOT_GIVEN",
    "TERMINAL_STATUSES",
    "JobResult",
    "ExtractJobResult",
    # errors
    "ScrapixError",
    "APIConnectionError",
    "APITimeoutError",
    "APIStatusError",
    "BadRequestError",
    "AuthenticationError",
    "InsufficientCreditsError",
    "PermissionDeniedError",
    "NotFoundError",
    "ConflictError",
    "UnprocessableEntityError",
    "RateLimitError",
    "InternalServerError",
    "JobTimeoutError",
    # most used models (all of them: scrapix.models)
    "BatchScrapeRequest",
    "BatchScrapeResponse",
    "CrawlConfig",
    "CrawlSyncResponse",
    "CreateCrawlResponse",
    "CreateExtractResponse",
    "ExtractRequest",
    "ExtractStatusResponse",
    "JobKind",
    "JobResultItem",
    "JobResultsResponse",
    "JobStatus",
    "JobStatusResponse",
    "MapRequest",
    "MapResponse",
    "ScrapeFormat",
    "ScrapeRequest",
    "ScrapeResponse",
    "SearchRequest",
]
