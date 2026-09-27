import { readFileSync } from "node:fs";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  APIConnectionError,
  APIError,
  APITimeoutError,
  AuthenticationError,
  BadRequestError,
  ConflictError,
  InsufficientCreditsError,
  InternalServerError,
  JobTimeoutError,
  NotFoundError,
  RateLimitError,
  Scrapix,
  VERSION,
  parseRetryAfter,
  type JobStatusResponse,
} from "../src/index.js";
import { BASE, MockAPI, jobStatus, json, resultsPage } from "./mock.js";

const SCRAPE_OK = { success: true, url: "https://example.com", status_code: 200, scrape_duration_ms: 3 };
const CREATED = { job_id: "job-1", status: "pending", index_uid: "i", start_urls_count: 1, message: "ok" };

afterEach(() => {
  vi.unstubAllEnvs();
});

describe("configuration and auth", () => {
  it("sends API keys as X-API-Key", async () => {
    const api = new MockAPI().add("POST", "/scrape", json(200, SCRAPE_OK));
    await api.client({ apiKey: "sk_live_abc" }).scrape("https://example.com");
    const [request] = api.requests;
    expect(request?.headers.get("x-api-key")).toBe("sk_live_abc");
    expect(request?.headers.get("authorization")).toBeNull();
    expect(request?.headers.get("user-agent")).toBe(`scrapix-js/${VERSION}`);
    expect(request?.headers.get("content-type")).toBe("application/json");
  });

  it("sends OAuth access tokens as Bearer", async () => {
    const api = new MockAPI().add("POST", "/scrape", json(200, SCRAPE_OK));
    await api.client({ apiKey: "oauth-token" }).scrape("https://example.com");
    expect(api.requests[0]?.headers.get("authorization")).toBe("Bearer oauth-token");
    expect(api.requests[0]?.headers.get("x-api-key")).toBeNull();
  });

  it("reads SCRAPIX_API_KEY and SCRAPIX_API_URL", async () => {
    vi.stubEnv("SCRAPIX_API_KEY", "sk_test_env");
    vi.stubEnv("SCRAPIX_API_URL", "https://self-hosted.test/");
    const api = new MockAPI().add("GET", "/health", json(200, { status: "ok", version: "1", kafka_connected: true }));
    const client = new Scrapix({ fetch: api.fetch });
    expect(client.baseUrl).toBe("https://self-hosted.test");
    await client.health();
    expect(api.requests[0]?.url.toString()).toBe("https://self-hosted.test/health");
    expect(api.requests[0]?.headers.get("x-api-key")).toBe("sk_test_env");
  });

  it("defaults to the public API and no credentials", async () => {
    vi.stubEnv("SCRAPIX_API_KEY", "");
    vi.stubEnv("SCRAPIX_API_URL", "");
    const api = new MockAPI().add("GET", "/health", json(200, { status: "ok", version: "1", kafka_connected: true }));
    const client = new Scrapix({ fetch: api.fetch });
    expect(client.baseUrl).toBe("https://scrapix.meilisearch.dev");
    await client.health();
    expect(api.requests[0]?.headers.get("x-api-key")).toBeNull();
    expect(api.requests[0]?.headers.get("authorization")).toBeNull();
  });

  it("keeps VERSION in sync with package.json", () => {
    const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string };
    expect(VERSION).toBe(pkg.version);
  });
});

describe("request bodies", () => {
  it("scrape merges the url into the params", async () => {
    const api = new MockAPI().add("POST", "/scrape", json(200, { ...SCRAPE_OK, markdown: "# Hi" }));
    const page = await api.client().scrape("https://example.com", {
      formats: ["markdown", "metadata"],
      ai: { summary: true },
    });
    expect(page.markdown).toBe("# Hi");
    expect(api.requests[0]?.body).toEqual({
      url: "https://example.com",
      formats: ["markdown", "metadata"],
      ai: { summary: true },
    });
  });

  it("crawlSync sends its query parameters", async () => {
    const api = new MockAPI().add(
      "POST",
      "/crawl/sync",
      json(200, { ...jobStatus("job-1", "completed"), results: resultsPage(["a"], null) }),
    );
    const res = await api.client().crawlSync({ start_urls: ["https://a.test"] }, { include_results: true, results_limit: 5 });
    expect(res.results?.data[0]?.url).toBe("a");
    const url = api.requests[0]?.url;
    expect(url?.searchParams.get("include_results")).toBe("true");
    expect(url?.searchParams.get("results_limit")).toBe("5");
  });

  it("job control and listing", async () => {
    const api = new MockAPI()
      .add("GET", "/jobs", json(200, [jobStatus("a"), jobStatus("b")]))
      .add("POST", "/job/a/pause", json(200, jobStatus("a", "paused")))
      .add("POST", "/job/a/resume", json(200, jobStatus("a", "running")))
      .add("DELETE", "/job/a", json(200, jobStatus("a", "cancelled")));
    const client = api.client();
    const jobs = await client.listJobs({ limit: 2 });
    expect(jobs.map((j) => j.job_id)).toEqual(["a", "b"]);
    expect(api.requests[0]?.url.searchParams.get("limit")).toBe("2");
    expect(api.requests[0]?.url.searchParams.has("offset")).toBe(false);
    expect((await client.pauseJob("a")).status).toBe("paused");
    expect((await client.resumeJob("a")).status).toBe("running");
    expect((await client.cancelJob("a")).status).toBe("cancelled");
  });

  it("escapes job ids in paths", async () => {
    const api = new MockAPI().add("GET", "/job/a%2Fb/status", json(200, jobStatus("a/b")));
    await api.client().getJob("a/b");
    expect(api.requests[0]?.url.pathname).toBe("/job/a%2Fb/status");
  });
});

describe("errors", () => {
  it.each([
    [400, "validation_error", BadRequestError],
    [401, "invalid_api_key", AuthenticationError],
    [402, "insufficient_credits", InsufficientCreditsError],
    [404, "not_found", NotFoundError],
    [409, "conflict", ConflictError],
    [500, "internal", InternalServerError],
  ] as const)("maps %i to a typed error", async (status, code, ErrorType) => {
    const api = new MockAPI().add("GET", "/job/x/status", json(status, { error: "Nope", code, details: { f: 1 } }));
    const error = await api.client({ maxRetries: 0 }).getJob("x").catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ErrorType);
    expect(error).toBeInstanceOf(APIError);
    const e = error as APIError;
    expect(e.status).toBe(status);
    expect(e.code).toBe(code);
    expect(e.apiMessage).toBe("Nope");
    expect(e.details).toEqual({ f: 1 });
    expect(e.message).toBe(`${status} ${code}: Nope`);
    expect(e.name).toBe(ErrorType.name);
  });

  it("handles non-JSON error bodies", async () => {
    const api = new MockAPI().add("POST", "/scrape", new Response("<html>Bad Gateway</html>", { status: 502 }));
    const error = (await api.client({ maxRetries: 0 }).scrape("https://x.test").catch((e: unknown) => e)) as APIError;
    expect(error).toBeInstanceOf(InternalServerError);
    expect(error.code).toBeUndefined();
    expect(error.apiMessage).toBe("<html>Bad Gateway</html>");
  });

  it("wraps network failures", async () => {
    const api = new MockAPI().add("GET", "/health", new TypeError("fetch failed"));
    await expect(api.client({ maxRetries: 0 }).health()).rejects.toBeInstanceOf(APIConnectionError);
  });

  it("times out slow requests", async () => {
    const slowFetch = (_url: string, init: RequestInit) =>
      new Promise<Response>((_resolve, reject) => {
        init.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")));
      });
    const client = new Scrapix({ baseUrl: BASE, fetch: slowFetch, timeoutMs: 10, maxRetries: 0 });
    await expect(client.health()).rejects.toBeInstanceOf(APITimeoutError);
  });
});

describe("retries", () => {
  it("retries 429 honoring Retry-After", async () => {
    const api = new MockAPI().add(
      "POST",
      "/scrape",
      json(429, { error: "Rate limit exceeded", code: "rate_limit_exceeded" }, { "retry-after": "3" }),
      json(200, SCRAPE_OK),
    );
    const client = api.client();
    const page = await client.scrape("https://example.com");
    expect(page.success).toBe(true);
    expect(api.requests).toHaveLength(2);
    expect(client.sleeps).toEqual([3000]);
  });

  it("throws RateLimitError once retries are exhausted", async () => {
    const api = new MockAPI().add(
      "POST",
      "/crawl",
      json(429, { error: "Rate limited", code: "rate_limit_exceeded", retry_after_seconds: 1 }, { "retry-after": "1" }),
    );
    const client = api.client({ maxRetries: 2 });
    const error = (await client.crawl({ start_urls: ["https://a.test"] }).catch((e: unknown) => e)) as RateLimitError;
    expect(error).toBeInstanceOf(RateLimitError);
    expect(error.retryAfter).toBe(1);
    expect(api.requests).toHaveLength(3);
    expect(client.sleeps).toEqual([1000, 1000]);
  });

  it("retries 5xx with exponential backoff on safe requests", async () => {
    const api = new MockAPI().add(
      "GET",
      "/job/j/status",
      json(503, { error: "down", code: "service_unavailable" }),
      new Response("bad gateway", { status: 502 }),
      json(200, jobStatus("j")),
    );
    const client = api.client();
    expect((await client.getJob("j")).job_id).toBe("j");
    expect(api.requests).toHaveLength(3);
    expect(client.sleeps[0]).toBeGreaterThanOrEqual(375);
    expect(client.sleeps[0]).toBeLessThanOrEqual(625);
    expect(client.sleeps[1]).toBeGreaterThanOrEqual(750);
    expect(client.sleeps[1]).toBeLessThanOrEqual(1250);
  });

  it("does not retry job creation on 5xx", async () => {
    const api = new MockAPI().add("POST", "/crawl", json(503, { error: "down", code: "x" }));
    const client = api.client();
    await expect(client.crawl({ start_urls: ["https://a.test"] })).rejects.toBeInstanceOf(InternalServerError);
    expect(api.requests).toHaveLength(1);
    expect(client.sleeps).toEqual([]);
  });

  it("retries network errors on safe requests", async () => {
    const api = new MockAPI().add(
      "GET",
      "/health",
      new TypeError("fetch failed"),
      json(200, { status: "ok", version: "1", kafka_connected: true }),
    );
    expect((await api.client().health()).status).toBe("ok");
    expect(api.requests).toHaveLength(2);
  });

  it("parses Retry-After dates", () => {
    const inThirty = new Date(Date.now() + 30_000).toUTCString();
    const seconds = parseRetryAfter(inThirty) ?? 0;
    expect(seconds).toBeGreaterThan(25);
    expect(seconds).toBeLessThanOrEqual(30);
    expect(parseRetryAfter("7")).toBe(7);
    expect(parseRetryAfter(null)).toBeUndefined();
    expect(parseRetryAfter("soon")).toBeUndefined();
  });
});

describe("pagination", () => {
  it("follows the cursor until next is null", async () => {
    const pages: Record<string, unknown> = {
      "": resultsPage(["u1", "u2"], "c1"),
      c1: resultsPage(["u3", "u4"], "c2"),
      c2: resultsPage(["u5"], null),
    };
    const api = new MockAPI().add("GET", "/job/job-1/results", (request) => {
      const cursor = new URL(request.url).searchParams.get("cursor") ?? "";
      return json(200, pages[cursor]);
    });
    const urls: string[] = [];
    for await (const item of api.client().iterJobResults("job-1", { pageSize: 2 })) urls.push(item.url);
    expect(urls).toEqual(["u1", "u2", "u3", "u4", "u5"]);
    expect(api.requests.map((r) => r.url.searchParams.get("cursor"))).toEqual([null, "c1", "c2"]);
    expect(api.requests.every((r) => r.url.searchParams.get("limit") === "2")).toBe(true);
  });

  it("stops once caught up with a running job", async () => {
    const api = new MockAPI().add(
      "GET",
      "/job/job-1/results",
      json(200, resultsPage(["u1"], "c1", "running")),
      json(200, resultsPage([], "c1", "running")),
    );
    const urls: string[] = [];
    for await (const item of api.client().iterJobResults("job-1")) urls.push(item.url);
    expect(urls).toEqual(["u1"]);
    expect(api.requests).toHaveLength(2);
  });

  it("with wait: true, polls until the job is terminal", async () => {
    const api = new MockAPI().add(
      "GET",
      "/job/job-1/results",
      json(200, resultsPage(["u1"], "c1", "running")),
      json(200, resultsPage([], "c1", "running")),
      json(200, resultsPage(["u2"], "c2", "running")),
      json(200, resultsPage([], null, "completed")),
    );
    const client = api.client();
    const urls: string[] = [];
    for await (const item of client.iterJobResults("job-1", { wait: true, pollIntervalMs: 1500 })) urls.push(item.url);
    expect(urls).toEqual(["u1", "u2"]);
    expect(api.requests.map((r) => r.url.searchParams.get("cursor"))).toEqual([null, "c1", "c1", "c2"]);
    expect(client.sleeps).toEqual([1500]);
  });
});

describe("job watcher", () => {
  it("waitForJob polls until terminal and reports progress", async () => {
    const api = new MockAPI().add(
      "GET",
      "/job/job-1/status",
      json(200, jobStatus("job-1", "pending")),
      json(200, jobStatus("job-1", "running", { pages_crawled: 4 })),
      json(200, jobStatus("job-1", "paused", { pages_crawled: 4 })),
      json(200, jobStatus("job-1", "completed", { pages_crawled: 9 })),
    );
    const client = api.client();
    const seen: string[] = [];
    const final = await client.waitForJob("job-1", {
      pollIntervalMs: 500,
      onProgress: (s: JobStatusResponse) => {
        seen.push(s.status);
      },
    });
    expect(final.status).toBe("completed");
    expect(final.pages_crawled).toBe(9);
    expect(seen).toEqual(["pending", "running", "paused", "completed"]);
    expect(client.sleeps).toEqual([500, 500, 500]);
  });

  it.each(["failed", "cancelled"])("resolves with %s jobs", async (terminal) => {
    const api = new MockAPI().add("GET", "/job/job-1/status", json(200, jobStatus("job-1", terminal)));
    expect((await api.client().waitForJob("job-1")).status).toBe(terminal);
  });

  it("throws JobTimeoutError after timeoutMs", async () => {
    const api = new MockAPI().add("GET", "/job/job-1/status", json(200, jobStatus("job-1", "running")));
    const error = (await api.client().waitForJob("job-1", { timeoutMs: 0 }).catch((e: unknown) => e)) as JobTimeoutError;
    expect(error).toBeInstanceOf(JobTimeoutError);
    expect(error.jobId).toBe("job-1");
    expect((error.lastStatus as JobStatusResponse).status).toBe("running");
  });

  it("watchJob yields every status", async () => {
    const api = new MockAPI().add(
      "GET",
      "/job/job-1/status",
      json(200, jobStatus("job-1", "running", { pages_crawled: 1 })),
      json(200, jobStatus("job-1", "completed", { pages_crawled: 2 })),
    );
    const counts: number[] = [];
    for await (const s of api.client().watchJob("job-1")) counts.push(s.pages_crawled);
    expect(counts).toEqual([1, 2]);
  });

  it("crawlAndWait returns the final status and every document", async () => {
    const api = new MockAPI()
      .add("POST", "/crawl", json(200, CREATED))
      .add("GET", "/job/job-1/status", json(200, jobStatus("job-1", "running")), json(200, jobStatus("job-1", "completed")))
      .add("GET", "/job/job-1/results", json(200, resultsPage(["a", "b"], "c1")), json(200, resultsPage(["c"], null)));
    const progress: string[] = [];
    const { job, documents } = await api.client().crawlAndWait(
      { start_urls: ["https://a.test"], max_pages: 3 },
      { onProgress: (s) => void progress.push(s.status) },
    );
    expect(job.status).toBe("completed");
    expect(documents.map((d) => d.url)).toEqual(["a", "b", "c"]);
    expect(progress).toEqual(["running", "completed"]);
    expect(api.calls("POST", "/crawl")[0]?.body).toEqual({ start_urls: ["https://a.test"], max_pages: 3 });
  });

  it("batchScrapeAndWait", async () => {
    const api = new MockAPI()
      .add("POST", "/batch/scrape", json(200, { job_id: "b-1", status: "running", urls_count: 2, message: "ok" }))
      .add("GET", "/job/b-1/status", json(200, jobStatus("b-1", "completed", { job_type: "batch_scrape" })))
      .add("GET", "/job/b-1/results", json(200, resultsPage(["x", "y"], null, "completed", "b-1")));
    const { documents } = await api.client().batchScrapeAndWait({
      urls: ["https://x.test", "https://y.test"],
      formats: ["markdown"],
    });
    expect(documents.map((d) => d.url)).toEqual(["x", "y"]);
  });

  it("extractAndWait returns the extraction", async () => {
    const api = new MockAPI()
      .add("POST", "/extract", json(200, { job_id: "ex-1", status: "running" }))
      .add("GET", "/job/ex-1/status", json(200, jobStatus("ex-1", "completed", { job_type: "extract" })))
      .add("GET", "/job/ex-1/results", json(200, resultsPage(["p"], null, "completed", "ex-1")))
      .add(
        "GET",
        "/extract/ex-1",
        json(200, { job_id: "ex-1", status: "completed", sources: [{ url: "p", success: true }], data: { title: "Hello" } }),
      );
    const result = await api.client().extractAndWait({ urls: ["https://p.test"], prompt: "title" });
    expect(result.extract.data).toEqual({ title: "Hello" });
    expect(result.documents.map((d) => d.url)).toEqual(["p"]);
    expect(api.calls("POST", "/extract")[0]?.body).toEqual({ urls: ["https://p.test"], prompt: "title" });
  });
});
