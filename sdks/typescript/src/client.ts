import {
  APIConnectionError,
  APITimeoutError,
  APIUserAbortError,
  JobTimeoutError,
  errorFromResponse,
} from "./errors.js";
import {
  isTerminalStatus,
  type BatchScrapeRequest,
  type BatchScrapeResponse,
  type CrawlConfig,
  type CrawlSyncQuery,
  type CrawlSyncResponse,
  type CreateCrawlResponse,
  type CreateExtractResponse,
  type ExtractJobResult,
  type ExtractRequest,
  type ExtractStatusResponse,
  type HealthResponse,
  type JobResult,
  type JobResultItem,
  type JobResultsResponse,
  type JobStatusResponse,
  type MapRequest,
  type MapResponse,
  type ScrapeRequest,
  type ScrapeResponse,
  type SearchRequest,
  type SearchResponse,
} from "./types.js";
import { VERSION } from "./version.js";

export const DEFAULT_BASE_URL = "https://scrapix.meilisearch.dev";
export const DEFAULT_TIMEOUT_MS = 60_000;
export const DEFAULT_MAX_RETRIES = 2;
/** `POST /crawl/sync` blocks until the crawl ends (server-side limit: 1 hour). */
export const CRAWL_SYNC_TIMEOUT_MS = 3_600_000;
export const DEFAULT_POLL_INTERVAL_MS = 2_000;
/** Page size used when iterating over job results (the API maximum). */
export const DEFAULT_RESULTS_PAGE_SIZE = 100;

const RETRYABLE_STATUSES = new Set([408, 429, 500, 502, 503, 504]);
const MAX_RETRY_AFTER_MS = 60_000;
const BACKOFF_BASE_MS = 500;
const BACKOFF_MAX_MS = 8_000;

type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

export interface ScrapixOptions {
  /**
   * An API key (`sk_live_...`, sent as `X-API-Key`) or an OAuth access token
   * (sent as `Authorization: Bearer`). Defaults to `SCRAPIX_API_KEY`. May be
   * omitted for a self-hosted engine running without authentication.
   */
  apiKey?: string;
  /** API root. Defaults to `SCRAPIX_API_URL`, then `https://scrapix.meilisearch.dev`. */
  baseUrl?: string;
  /** Per-request timeout in milliseconds (default 60 000; `0` disables it). */
  timeoutMs?: number;
  /**
   * Retries on 429 (honoring `Retry-After`) and, for requests that are safe
   * to repeat, on 408/5xx and network errors (default 2). Requests that
   * create or change a job are only retried on 429.
   */
  maxRetries?: number;
  /** Extra headers sent with every request. */
  headers?: Record<string, string>;
  /** Custom `fetch` implementation (defaults to the global one). */
  fetch?: FetchLike;
}

/** Per-call overrides. */
export interface RequestOptions {
  timeoutMs?: number;
  maxRetries?: number;
  signal?: AbortSignal;
}

export interface WaitOptions {
  /** Delay between two status polls (default 2000 ms). */
  pollIntervalMs?: number;
  /** Give up (throw `JobTimeoutError`) after this long. Default: wait forever. */
  timeoutMs?: number;
  /** Called with every status polled, including the final one. */
  onProgress?: (status: JobStatusResponse) => void | Promise<void>;
  signal?: AbortSignal;
}

export interface WaitAndCollectOptions extends WaitOptions {
  /** Page size used to read the results (default 100, the API maximum). */
  pageSize?: number;
}

export interface IterJobResultsOptions {
  /** Page size (default 100, the API maximum). */
  pageSize?: number;
  /**
   * For a running job: keep polling for new results until the job is
   * terminal (default `false`: stop once caught up with the results
   * available so far).
   */
  wait?: boolean;
  pollIntervalMs?: number;
  signal?: AbortSignal;
}

interface InternalRequest {
  method: "GET" | "POST" | "DELETE";
  path: string;
  body?: unknown;
  query?: Record<string, string | number | boolean | undefined | null>;
  /** Safe to repeat: retried on 408/5xx and network errors too. */
  idempotent?: boolean;
  timeoutMs?: number;
}

type Without<T, K extends keyof T> = Omit<T, K> & Partial<Pick<T, K>>;

function readEnv(name: string): string | undefined {
  const g = globalThis as { process?: { env?: Record<string, string | undefined> } };
  const value = g.process?.env?.[name];
  return value === "" ? undefined : value;
}

function authHeaders(key: string): Record<string, string> {
  // Both backends read `Authorization: Bearer` as an OAuth access token only:
  // API keys must go in `X-API-Key`.
  return key.startsWith("sk_") ? { "X-API-Key": key } : { Authorization: `Bearer ${key}` };
}

/** Seconds requested by a `Retry-After` header (delta-seconds or HTTP date). */
export function parseRetryAfter(value: string | null): number | undefined {
  if (!value) return undefined;
  const seconds = Number(value);
  if (!Number.isNaN(seconds)) return Math.max(0, seconds);
  const date = Date.parse(value);
  if (Number.isNaN(date)) return undefined;
  return Math.max(0, (date - Date.now()) / 1000);
}

function retryDelayMs(attempt: number, retryAfterSeconds: number | undefined): number {
  if (retryAfterSeconds !== undefined) {
    return Math.min(retryAfterSeconds * 1000, MAX_RETRY_AFTER_MS);
  }
  const backoff = Math.min(BACKOFF_BASE_MS * 2 ** attempt, BACKOFF_MAX_MS);
  return backoff * (0.75 + Math.random() * 0.5);
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(new APIUserAbortError("aborted"));
      return;
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      reject(new APIUserAbortError("aborted"));
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

function compact<T extends Record<string, unknown>>(value: T): T {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(value)) if (v !== undefined) out[k] = v;
  return out as T;
}

function jobPath(jobId: string, suffix = ""): string {
  return `/job/${encodeURIComponent(jobId)}${suffix}`;
}

/**
 * Client for the Scrapix API.
 *
 * ```ts
 * const scrapix = new Scrapix(); // SCRAPIX_API_KEY / SCRAPIX_API_URL
 * const page = await scrapix.scrape("https://example.com", { formats: ["markdown"] });
 * const { job, documents } = await scrapix.crawlAndWait({ start_urls: ["https://docs.example.com"] });
 * ```
 */
export class Scrapix {
  readonly baseUrl: string;
  private readonly headers: Record<string, string>;
  private readonly timeoutMs: number;
  private readonly maxRetries: number;
  private readonly fetchImpl: FetchLike;

  /** Replaceable for tests. */
  protected sleep: (ms: number, signal?: AbortSignal) => Promise<void> = sleep;

  constructor(options: ScrapixOptions = {}) {
    const key = options.apiKey ?? readEnv("SCRAPIX_API_KEY");
    const baseUrl = options.baseUrl ?? readEnv("SCRAPIX_API_URL") ?? DEFAULT_BASE_URL;
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.maxRetries = Math.max(0, options.maxRetries ?? DEFAULT_MAX_RETRIES);
    const fetchImpl = options.fetch ?? (globalThis.fetch as FetchLike | undefined);
    if (!fetchImpl) {
      throw new Error("scrapix: no global fetch available (Node >= 18); pass `fetch` in the options");
    }
    this.fetchImpl = fetchImpl;
    const isBrowser = typeof (globalThis as { document?: unknown }).document !== "undefined";
    this.headers = {
      Accept: "application/json",
      // Browsers refuse to set User-Agent; only send it server-side.
      ...(isBrowser ? {} : { "User-Agent": `scrapix-js/${VERSION}` }),
      ...(key ? authHeaders(key) : {}),
      ...options.headers,
    };
  }

  // ------------------------------------------------------------------
  // Transport
  // ------------------------------------------------------------------

  private async request<T>(req: InternalRequest, options: RequestOptions = {}): Promise<T> {
    const url = new URL(this.baseUrl + req.path);
    for (const [k, v] of Object.entries(req.query ?? {})) {
      if (v !== undefined && v !== null) url.searchParams.set(k, String(v));
    }
    const maxRetries = Math.max(0, options.maxRetries ?? this.maxRetries);
    const timeoutMs = options.timeoutMs ?? req.timeoutMs ?? this.timeoutMs;
    const idempotent = req.idempotent ?? true;
    const headers: Record<string, string> = { ...this.headers };
    if (req.body !== undefined) headers["Content-Type"] = "application/json";

    for (let attempt = 0; ; attempt++) {
      if (options.signal?.aborted) throw new APIUserAbortError("aborted");
      const controller = new AbortController();
      const onAbort = () => controller.abort();
      options.signal?.addEventListener("abort", onAbort, { once: true });
      let timedOut = false;
      const timer =
        timeoutMs > 0
          ? setTimeout(() => {
              timedOut = true;
              controller.abort();
            }, timeoutMs)
          : undefined;

      let response: Response;
      try {
        response = await this.fetchImpl(url.toString(), {
          method: req.method,
          headers,
          body: req.body === undefined ? undefined : JSON.stringify(req.body),
          signal: controller.signal,
        });
      } catch (error) {
        if (options.signal?.aborted) throw new APIUserAbortError("aborted", { cause: error });
        if (attempt < maxRetries && idempotent) {
          await this.sleep(retryDelayMs(attempt, undefined), options.signal);
          continue;
        }
        if (timedOut) {
          throw new APITimeoutError(`request timed out after ${timeoutMs}ms`, { cause: error });
        }
        throw new APIConnectionError(`connection error: ${String(error)}`, { cause: error });
      } finally {
        if (timer !== undefined) clearTimeout(timer);
        options.signal?.removeEventListener("abort", onAbort);
      }

      if (response.ok) {
        if (response.status === 204) return undefined as T;
        const text = await response.text();
        if (!text) return undefined as T;
        try {
          return JSON.parse(text) as T;
        } catch {
          return text as T;
        }
      }

      const retryAfter = parseRetryAfter(response.headers.get("retry-after"));
      const retryable =
        response.status === 429 || (idempotent && RETRYABLE_STATUSES.has(response.status));
      if (attempt < maxRetries && retryable) {
        // Drain the body so the connection can be reused.
        await response.text().catch(() => undefined);
        await this.sleep(retryDelayMs(attempt, retryAfter), options.signal);
        continue;
      }
      const text = await response.text().catch(() => "");
      let body: unknown = text;
      try {
        body = text ? JSON.parse(text) : undefined;
      } catch {
        // keep the raw text
      }
      throw errorFromResponse(response.status, response.statusText, response.headers, body, retryAfter);
    }
  }

  // ------------------------------------------------------------------
  // Single-request endpoints
  // ------------------------------------------------------------------

  /** `GET /health`. */
  health(options?: RequestOptions): Promise<HealthResponse> {
    return this.request({ method: "GET", path: "/health" }, options);
  }

  /** Scrape one page (`POST /scrape`). `params`: any `ScrapeRequest` field besides `url`. */
  scrape(
    url: string,
    params: Without<ScrapeRequest, "url"> = {},
    options?: RequestOptions,
  ): Promise<ScrapeResponse> {
    return this.request({ method: "POST", path: "/scrape", body: { ...params, url } }, options);
  }

  /** Discover a site's URLs (`POST /map`). */
  map(url: string, params: Without<MapRequest, "url"> = {}, options?: RequestOptions): Promise<MapResponse> {
    return this.request({ method: "POST", path: "/map", body: { ...params, url } }, options);
  }

  /** Search the content of a site (`POST /search`); returns the decoded JSON. */
  search(
    url: string,
    q: string,
    params: Without<SearchRequest, "url" | "q"> = {},
    options?: RequestOptions,
  ): Promise<SearchResponse> {
    return this.request({ method: "POST", path: "/search", body: { ...params, url, q } }, options);
  }

  // ------------------------------------------------------------------
  // Jobs: creation
  // ------------------------------------------------------------------

  /** Start a distributed crawl (`POST /crawl`); returns immediately. */
  crawl(config: CrawlConfig, options?: RequestOptions): Promise<CreateCrawlResponse> {
    return this.request({ method: "POST", path: "/crawl", body: config, idempotent: false }, options);
  }

  /**
   * Run a crawl and block until it ends (`POST /crawl/sync`). With
   * `{ include_results: true }` the response carries the first page of
   * results. Prefer `crawlAndWait` for long crawls.
   */
  crawlSync(
    config: CrawlConfig,
    query: CrawlSyncQuery = {},
    options?: RequestOptions,
  ): Promise<CrawlSyncResponse> {
    return this.request(
      {
        method: "POST",
        path: "/crawl/sync",
        body: config,
        query: compact({ ...query }),
        idempotent: false,
        timeoutMs: CRAWL_SYNC_TIMEOUT_MS,
      },
      options,
    );
  }

  /**
   * Scrape many URLs as one job (`POST /batch/scrape`); returns immediately.
   * Every `/scrape` option in `request` applies to each URL.
   */
  batchScrape(request: BatchScrapeRequest, options?: RequestOptions): Promise<BatchScrapeResponse> {
    return this.request(
      { method: "POST", path: "/batch/scrape", body: request, idempotent: false },
      options,
    );
  }

  /** Start a structured extraction over pages (`POST /extract`). */
  extract(request: ExtractRequest, options?: RequestOptions): Promise<CreateExtractResponse> {
    return this.request({ method: "POST", path: "/extract", body: request, idempotent: false }, options);
  }

  /** `GET /extract/{id}`: status, sources and (once completed) `data`. */
  getExtract(jobId: string, options?: RequestOptions): Promise<ExtractStatusResponse> {
    return this.request({ method: "GET", path: `/extract/${encodeURIComponent(jobId)}` }, options);
  }

  // ------------------------------------------------------------------
  // Jobs: status and control
  // ------------------------------------------------------------------

  /** `GET /job/{id}/status`. */
  getJob(jobId: string, options?: RequestOptions): Promise<JobStatusResponse> {
    return this.request({ method: "GET", path: jobPath(jobId, "/status") }, options);
  }

  /** `GET /jobs`: the account's jobs, newest first. */
  listJobs(
    query: { limit?: number; offset?: number } = {},
    options?: RequestOptions,
  ): Promise<JobStatusResponse[]> {
    return this.request({ method: "GET", path: "/jobs", query }, options);
  }

  /** `DELETE /job/{id}`: cancel a running or paused job. */
  cancelJob(jobId: string, options?: RequestOptions): Promise<JobStatusResponse> {
    return this.request({ method: "DELETE", path: jobPath(jobId), idempotent: false }, options);
  }

  /** `POST /job/{id}/pause`. */
  pauseJob(jobId: string, options?: RequestOptions): Promise<JobStatusResponse> {
    return this.request({ method: "POST", path: jobPath(jobId, "/pause"), idempotent: false }, options);
  }

  /** `POST /job/{id}/resume`. */
  resumeJob(jobId: string, options?: RequestOptions): Promise<JobStatusResponse> {
    return this.request({ method: "POST", path: jobPath(jobId, "/resume"), idempotent: false }, options);
  }

  // ------------------------------------------------------------------
  // Jobs: results
  // ------------------------------------------------------------------

  /**
   * One page of `GET /job/{id}/results` (`limit` <= 100). `next` is the
   * cursor of the following page; it is `null` only once the job is
   * terminal and every result was returned.
   */
  jobResults(
    jobId: string,
    query: { limit?: number; cursor?: string | null } = {},
    options?: RequestOptions,
  ): Promise<JobResultsResponse> {
    return this.request({ method: "GET", path: jobPath(jobId, "/results"), query }, options);
  }

  /**
   * Iterate over every result of a job, following the cursor.
   *
   * For a finished job this yields all results then stops. For a running job
   * it stops once caught up with the results available so far, unless
   * `wait: true`: it then keeps polling and stops when the job is terminal
   * and every result was yielded.
   */
  async *iterJobResults(jobId: string, opts: IterJobResultsOptions = {}): AsyncGenerator<JobResultItem> {
    const limit = opts.pageSize ?? DEFAULT_RESULTS_PAGE_SIZE;
    let cursor: string | undefined;
    for (;;) {
      const page = await this.jobResults(jobId, { limit, cursor }, { signal: opts.signal });
      yield* page.data;
      if (page.next === null || page.next === undefined) return;
      if (page.data.length === 0) {
        if (!opts.wait) return;
        await this.sleep(opts.pollIntervalMs ?? DEFAULT_POLL_INTERVAL_MS, opts.signal);
      }
      cursor = page.next;
    }
  }

  // ------------------------------------------------------------------
  // Job watcher
  // ------------------------------------------------------------------

  /**
   * Poll `GET /job/{id}/status` and yield each status, ending with the
   * terminal one (`completed`, `failed` or `cancelled`). Throws
   * `JobTimeoutError` when `timeoutMs` elapses first.
   */
  async *watchJob(jobId: string, opts: Omit<WaitOptions, "onProgress"> = {}): AsyncGenerator<JobStatusResponse> {
    const interval = opts.pollIntervalMs ?? DEFAULT_POLL_INTERVAL_MS;
    const deadline = opts.timeoutMs === undefined ? undefined : Date.now() + opts.timeoutMs;
    for (;;) {
      const status = await this.getJob(jobId, { signal: opts.signal });
      yield status;
      if (isTerminalStatus(status.status)) return;
      if (deadline !== undefined) {
        const remaining = deadline - Date.now();
        if (remaining <= 0) throw new JobTimeoutError(jobId, opts.timeoutMs ?? 0, status);
        await this.sleep(Math.min(interval, remaining), opts.signal);
      } else {
        await this.sleep(interval, opts.signal);
      }
    }
  }

  /**
   * Resolve with a job's final status once it is terminal. A failed or
   * cancelled job resolves (it does not reject): check `status`.
   */
  async waitForJob(jobId: string, opts: WaitOptions = {}): Promise<JobStatusResponse> {
    let last: JobStatusResponse | undefined;
    for await (const status of this.watchJob(jobId, opts)) {
      await opts.onProgress?.(status);
      last = status;
    }
    return last as JobStatusResponse;
  }

  private async collect(jobId: string, opts: WaitAndCollectOptions): Promise<JobResult> {
    const job = await this.waitForJob(jobId, opts);
    const documents: JobResultItem[] = [];
    for await (const item of this.iterJobResults(jobId, { pageSize: opts.pageSize, signal: opts.signal })) {
      documents.push(item);
    }
    return { job, documents };
  }

  /**
   * Start a crawl, wait for it to end, and resolve with its final status and
   * every document. Crawled pages are indexed into Meilisearch
   * asynchronously: the last documents of a crawl that just finished can
   * take a moment to be readable.
   */
  async crawlAndWait(config: CrawlConfig, opts: WaitAndCollectOptions = {}): Promise<JobResult> {
    const created = await this.crawl(config, { signal: opts.signal });
    return this.collect(created.job_id, opts);
  }

  /** Start a batch scrape, wait for it to end, and resolve with one document per URL. */
  async batchScrapeAndWait(request: BatchScrapeRequest, opts: WaitAndCollectOptions = {}): Promise<JobResult> {
    const created = await this.batchScrape(request, { signal: opts.signal });
    return this.collect(created.job_id, opts);
  }

  /**
   * Start an extraction, wait for it to end, and resolve with the extraction
   * (`extract.data`), its status and the pages it used.
   */
  async extractAndWait(request: ExtractRequest, opts: WaitAndCollectOptions = {}): Promise<ExtractJobResult> {
    const created = await this.extract(request, { signal: opts.signal });
    const collected = await this.collect(created.job_id, opts);
    const extract = await this.getExtract(created.job_id, { signal: opts.signal });
    return { ...collected, extract };
  }
}
