/** Base class of every error thrown by this SDK. */
export class ScrapixError extends Error {
  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = new.target.name;
  }
}

/** The API could not be reached (DNS, TCP, TLS, connection reset, ...). */
export class APIConnectionError extends ScrapixError {}

/** The request timed out. */
export class APITimeoutError extends APIConnectionError {}

/** The request was aborted through the caller's `AbortSignal`. */
export class APIUserAbortError extends ScrapixError {}

/** Decoded JSON (or raw text) body of an error response. */
export type ErrorBody = unknown;

/**
 * The API answered with a non-2xx status.
 *
 * `code` is the machine-readable error code from the body (`not_found`,
 * `validation_error`, `rate_limit_exceeded`, ...), `message` the body's
 * `error` field.
 */
export class APIError extends ScrapixError {
  /** The API's error message, without the status/code prefix. */
  readonly apiMessage: string;
  readonly status: number;
  readonly code: string | undefined;
  readonly details: unknown;
  readonly body: ErrorBody;
  readonly headers: Headers;

  constructor(
    message: string,
    init: { status: number; code?: string; details?: unknown; body: ErrorBody; headers: Headers },
  ) {
    super(`${init.status}${init.code ? ` ${init.code}` : ""}: ${message}`);
    this.status = init.status;
    this.code = init.code;
    this.details = init.details;
    this.body = init.body;
    this.headers = init.headers;
    this.apiMessage = message;
  }
}

/** 400: the request was rejected (`validation_error`, `bad_request`, ...). */
export class BadRequestError extends APIError {}
/** 401: missing, invalid or expired credentials. */
export class AuthenticationError extends APIError {}
/** 402: not enough credits (`insufficient_credits`). */
export class InsufficientCreditsError extends APIError {}
/** 403: forbidden (e.g. `spend_limit_exceeded`). */
export class PermissionDeniedError extends APIError {}
/** 404: the resource does not exist or is not owned by the caller. */
export class NotFoundError extends APIError {}
/** 409: the resource's state forbids the operation (e.g. pausing a finished job). */
export class ConflictError extends APIError {}
/** 422: the request was understood but cannot be processed. */
export class UnprocessableEntityError extends APIError {}
/** 429: too many requests. `retryAfter` is the delay (seconds) the API asked for. */
export class RateLimitError extends APIError {
  retryAfter: number | undefined;
}
/** 5xx: the API failed to process the request. */
export class InternalServerError extends APIError {}

/** A job did not reach a terminal status within the wait timeout. */
export class JobTimeoutError extends ScrapixError {
  readonly jobId: string;
  readonly timeoutMs: number;
  /** Last status read, if any. The job keeps running server-side. */
  readonly lastStatus: unknown;

  constructor(jobId: string, timeoutMs: number, lastStatus: unknown) {
    super(`job ${jobId} did not finish within ${timeoutMs}ms`);
    this.jobId = jobId;
    this.timeoutMs = timeoutMs;
    this.lastStatus = lastStatus;
  }
}

const STATUS_ERRORS: Record<number, typeof APIError> = {
  400: BadRequestError,
  401: AuthenticationError,
  402: InsufficientCreditsError,
  403: PermissionDeniedError,
  404: NotFoundError,
  409: ConflictError,
  422: UnprocessableEntityError,
  429: RateLimitError,
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Build the typed error for a non-2xx response. Error bodies are
 * `{ error, code, details? }`; the auth routes use `{ error, "field-error" }`.
 */
export function errorFromResponse(
  status: number,
  statusText: string,
  headers: Headers,
  body: ErrorBody,
  retryAfter?: number,
): APIError {
  let code: string | undefined;
  let message = "";
  let details: unknown;
  if (isRecord(body)) {
    if (typeof body.code === "string") code = body.code;
    const raw = body.error ?? body.message;
    if (typeof raw === "string") message = raw;
    details = body.details ?? body["field-error"];
  } else if (typeof body === "string") {
    message = body.trim().slice(0, 500);
  }
  if (!message) message = statusText || `HTTP ${status}`;

  const Ctor = STATUS_ERRORS[status] ?? (status >= 500 ? InternalServerError : APIError);
  const error = new Ctor(message, { status, code, details, body, headers });
  if (error instanceof RateLimitError) {
    let seconds = retryAfter;
    if (seconds === undefined && isRecord(body) && typeof body.retry_after_seconds === "number") {
      seconds = body.retry_after_seconds;
    }
    error.retryAfter = seconds;
  }
  return error;
}
