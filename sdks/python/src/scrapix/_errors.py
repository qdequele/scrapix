"""Exceptions raised by the Scrapix client."""

from __future__ import annotations

from typing import Any, Optional

import httpx

__all__ = [
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
]


class ScrapixError(Exception):
    """Base class of every error raised by this SDK."""


class APIConnectionError(ScrapixError):
    """The API could not be reached (DNS, TCP, TLS, connection reset...)."""

    def __init__(self, message: str, *, request: Optional[httpx.Request] = None) -> None:
        super().__init__(message)
        self.message = message
        self.request = request


class APITimeoutError(APIConnectionError):
    """The request timed out."""


class APIStatusError(ScrapixError):
    """The API answered with a non-2xx status.

    Attributes:
        status_code: HTTP status of the response.
        code: machine-readable error code from the body (``not_found``,
            ``validation_error``, ``rate_limit_exceeded``, ...), when present.
        message: human-readable message (the body's ``error`` field).
        details: the body's ``details`` field, when present.
        body: the decoded JSON body (or the raw text when it is not JSON).
        response: the underlying ``httpx.Response``.
    """

    def __init__(
        self,
        message: str,
        *,
        response: httpx.Response,
        body: Any,
        code: Optional[str] = None,
        details: Any = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.response = response
        self.status_code = response.status_code
        self.body = body
        self.code = code
        self.details = details

    @property
    def headers(self) -> httpx.Headers:
        return self.response.headers

    def __str__(self) -> str:
        code = f" {self.code}" if self.code else ""
        return f"{self.status_code}{code}: {self.message}"


class BadRequestError(APIStatusError):
    """400: the request was rejected (``validation_error``, ``bad_request``, ...)."""


class AuthenticationError(APIStatusError):
    """401: missing, invalid or expired credentials."""


class InsufficientCreditsError(APIStatusError):
    """402: the account does not have enough credits (``insufficient_credits``)."""


class PermissionDeniedError(APIStatusError):
    """403: forbidden (e.g. ``spend_limit_exceeded``)."""


class NotFoundError(APIStatusError):
    """404: the resource does not exist or is not owned by the caller."""


class ConflictError(APIStatusError):
    """409: the resource is in a state that forbids the operation
    (e.g. pausing a job that already finished)."""


class UnprocessableEntityError(APIStatusError):
    """422: the request was understood but cannot be processed."""


class RateLimitError(APIStatusError):
    """429: too many requests. ``retry_after`` is the delay (seconds) the API
    asked for, when it sent one."""

    def __init__(
        self,
        message: str,
        *,
        response: httpx.Response,
        body: Any,
        code: Optional[str] = None,
        details: Any = None,
        retry_after: Optional[float] = None,
    ) -> None:
        super().__init__(message, response=response, body=body, code=code, details=details)
        self.retry_after = retry_after


class InternalServerError(APIStatusError):
    """5xx: the API failed to process the request."""


class JobTimeoutError(ScrapixError):
    """A job did not reach a terminal status within the wait timeout.

    The job keeps running server-side; ``last_status`` is the last status
    read (``None`` if none could be read).
    """

    def __init__(self, job_id: str, timeout: float, last_status: Any = None) -> None:
        super().__init__(f"job {job_id} did not finish within {timeout:g}s")
        self.job_id = job_id
        self.timeout = timeout
        self.last_status = last_status


_STATUS_ERRORS: dict[int, type[APIStatusError]] = {
    400: BadRequestError,
    401: AuthenticationError,
    402: InsufficientCreditsError,
    403: PermissionDeniedError,
    404: NotFoundError,
    409: ConflictError,
    422: UnprocessableEntityError,
    429: RateLimitError,
}


def error_from_response(
    response: httpx.Response, retry_after: Optional[float] = None
) -> APIStatusError:
    """Build the typed error for a non-2xx response.

    Error bodies are ``{"error": str, "code": str, "details"?: any}`` (both
    backends); the auth routes use ``{"error": str, "field-error": [...]}``.
    """
    body: Any
    try:
        body = response.json()
    except ValueError:
        body = response.text

    code: Optional[str] = None
    details: Any = None
    message = ""
    if isinstance(body, dict):
        raw_code = body.get("code")
        code = raw_code if isinstance(raw_code, str) else None
        raw_message = body.get("error", body.get("message"))
        message = raw_message if isinstance(raw_message, str) else ""
        details = body.get("details", body.get("field-error"))
    elif isinstance(body, str):
        message = body.strip()[:500]
    if not message:
        message = response.reason_phrase or f"HTTP {response.status_code}"

    status = response.status_code
    if status == 429:
        if retry_after is None and isinstance(body, dict):
            seconds = body.get("retry_after_seconds")
            if isinstance(seconds, (int, float)):
                retry_after = float(seconds)
        return RateLimitError(
            message,
            response=response,
            body=body,
            code=code,
            details=details,
            retry_after=retry_after,
        )
    cls = _STATUS_ERRORS.get(status)
    if cls is None:
        cls = InternalServerError if status >= 500 else APIStatusError
    return cls(message, response=response, body=body, code=code, details=details)
