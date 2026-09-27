export {
  Scrapix,
  DEFAULT_BASE_URL,
  DEFAULT_MAX_RETRIES,
  DEFAULT_POLL_INTERVAL_MS,
  DEFAULT_RESULTS_PAGE_SIZE,
  DEFAULT_TIMEOUT_MS,
  CRAWL_SYNC_TIMEOUT_MS,
  parseRetryAfter,
  type ScrapixOptions,
  type RequestOptions,
  type WaitOptions,
  type WaitAndCollectOptions,
  type IterJobResultsOptions,
} from "./client.js";
export {
  ScrapixError,
  APIConnectionError,
  APITimeoutError,
  APIUserAbortError,
  APIError,
  BadRequestError,
  AuthenticationError,
  InsufficientCreditsError,
  PermissionDeniedError,
  NotFoundError,
  ConflictError,
  UnprocessableEntityError,
  RateLimitError,
  InternalServerError,
  JobTimeoutError,
} from "./errors.js";
export * from "./types.js";
export { VERSION } from "./version.js";
