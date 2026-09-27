import { Scrapix, type ScrapixOptions } from "../src/index.js";

export const BASE = "https://api.test";

type Reply = Response | Error | ((request: Request) => Response);

export interface Recorded {
  method: string;
  url: URL;
  headers: Headers;
  body: unknown;
}

/**
 * Scripted fetch: `add(method, path, ...replies)` queues replies for a route;
 * the last reply of a route repeats once the queue is drained.
 */
export class MockAPI {
  private routes = new Map<string, Reply[]>();
  readonly requests: Recorded[] = [];

  add(method: string, path: string, ...replies: Reply[]): this {
    const key = `${method} ${path}`;
    this.routes.set(key, [...(this.routes.get(key) ?? []), ...replies]);
    return this;
  }

  calls(method: string, path: string): Recorded[] {
    return this.requests.filter((r) => r.method === method && r.url.pathname === path);
  }

  fetch = async (input: string, init: RequestInit): Promise<Response> => {
    const request = new Request(input, init);
    const url = new URL(request.url);
    const text = await request.text();
    this.requests.push({
      method: request.method,
      url,
      headers: request.headers,
      body: text ? JSON.parse(text) : undefined,
    });
    const queue = this.routes.get(`${request.method} ${url.pathname}`);
    if (!queue || queue.length === 0) {
      return json(404, { error: "no route", code: "not_found" });
    }
    const reply = (queue.length > 1 ? queue.shift() : queue[0]) as Reply;
    if (reply instanceof Error) throw reply;
    if (typeof reply === "function") return reply(request);
    return reply.clone();
  };

  client(options: ScrapixOptions = {}): TestScrapix {
    return new TestScrapix({ apiKey: "sk_test_123", baseUrl: BASE, fetch: this.fetch, ...options });
  }
}

/** A client that records sleeps instead of sleeping. */
export class TestScrapix extends Scrapix {
  readonly sleeps: number[] = [];
  protected override sleep = async (ms: number): Promise<void> => {
    this.sleeps.push(ms);
  };
}

export function json(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

export function jobStatus(jobId = "job-1", status = "running", extra: Record<string, unknown> = {}) {
  return {
    job_id: jobId,
    job_type: "crawl",
    status,
    index_uid: "idx",
    pages_crawled: 0,
    pages_indexed: 0,
    documents_sent: 0,
    errors: 0,
    crawl_rate: 0,
    ...extra,
  };
}

export function resultsPage(urls: string[], next: string | null, status = "completed", jobId = "job-1") {
  return {
    job_id: jobId,
    job_type: "crawl",
    status,
    total: urls.length,
    next,
    data: urls.map((url) => ({ success: true, url })),
  };
}
