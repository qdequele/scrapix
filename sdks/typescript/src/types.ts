/**
 * Friendly aliases over the types generated from `contracts/openapi.json`
 * (`src/generated/schema.ts`). Every API schema is also reachable as
 * `Schemas["Name"]`.
 */
import type { components, operations, paths } from "./generated/schema.js";

export type { components, operations, paths };

/** Every schema of the API, by name. */
export type Schemas = components["schemas"];

export type ScrapeRequest = Schemas["ScrapeRequest"];
export type ScrapeResponse = Schemas["ScrapeResponse"];
export type ScrapeFormat = Schemas["ScrapeFormat"];
export type MapRequest = Schemas["MapRequest"];
export type MapResponse = Schemas["MapResponse"];
export type SearchRequest = Schemas["SearchRequest"];
export type CrawlConfig = Schemas["CrawlConfig"];
export type CreateCrawlResponse = Schemas["CreateCrawlResponse"];
export type CrawlSyncResponse = Schemas["CrawlSyncResponse"];
export type BatchScrapeRequest = Schemas["BatchScrapeRequest"];
export type BatchScrapeResponse = Schemas["BatchScrapeResponse"];
export type ExtractRequest = Schemas["ExtractRequest"];
export type CreateExtractResponse = Schemas["CreateExtractResponse"];
export type ExtractStatusResponse = Schemas["ExtractStatusResponse"];
export type JobStatus = Schemas["JobStatus"];
export type JobKind = Schemas["JobKind"];
export type JobStatusResponse = Schemas["JobStatusResponse"];
export type JobResultsResponse = Schemas["JobResultsResponse"];
export type JobResultItem = Schemas["JobResultItem"];
export type HealthResponse = Schemas["HealthResponse"];
export type WebhookConfig = Schemas["WebhookConfig"];

/** Query parameters of `POST /crawl/sync`. */
export type CrawlSyncQuery = NonNullable<operations["create_crawl_sync"]["parameters"]["query"]>;

/** `POST /search` has no response schema: the decoded JSON is returned. */
export type SearchResponse = Record<string, unknown>;

/** Statuses after which a job never changes again. */
export const TERMINAL_STATUSES: readonly JobStatus[] = ["completed", "failed", "cancelled"];

export function isTerminalStatus(status: JobStatus | string): boolean {
  return (TERMINAL_STATUSES as readonly string[]).includes(status);
}

/** Outcome of a job run with a `*AndWait` helper. */
export interface JobResult {
  /** Final job status (`job.status` is `completed`, `failed` or `cancelled`). */
  job: JobStatusResponse;
  /**
   * Every result the job produced, in order, each shaped like a `/scrape`
   * response (failed pages have `success: false` and an `error`).
   */
  documents: JobResultItem[];
}

/** Outcome of `extractAndWait`: `extract.data` holds the extraction. */
export interface ExtractJobResult extends JobResult {
  extract: ExtractStatusResponse;
}
