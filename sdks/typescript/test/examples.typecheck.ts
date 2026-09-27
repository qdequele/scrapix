/**
 * The README / docs examples, type-checked by `npm run typecheck` (never
 * executed) so they stay valid when the generated types change.
 */
import { NotFoundError, Scrapix } from "../src/index.js";

export async function examples(): Promise<void> {
  const scrapix = new Scrapix({ apiKey: "sk_live_..." });

  const page = await scrapix.scrape("https://example.com", { formats: ["markdown", "metadata"] });
  console.log(page.markdown, page.metadata);
  await scrapix.scrape("https://example.com", { only_main_content: true, render_js: true });
  await scrapix.map("https://example.com", { limit: 500 });
  await scrapix.search("https://docs.example.com", "rate limits", { limit: 5 });

  const { job, documents } = await scrapix.crawlAndWait(
    { start_urls: ["https://docs.example.com"], max_pages: 200 },
    { onProgress: (s) => console.log(s.status, s.pages_crawled) },
  );
  console.log(job.status, documents.length);

  const local = new Scrapix({ baseUrl: "http://localhost:8080" });
  await local.crawlAndWait(
    {
      start_urls: ["https://docs.meilisearch.com"],
      index_uid: "meilisearch-docs",
      max_depth: 5,
      max_pages: 1000,
      meilisearch: { url: "http://localhost:7700", api_key: "masterKey" },
    },
    { onProgress: (s) => console.log(s.status, s.pages_crawled) },
  );
  const synced = await local.crawlSync(
    { start_urls: ["https://example.com"], index_uid: "test", max_depth: 2, max_pages: 50 },
    { include_results: true },
  );
  console.log(synced.status, synced.pages_crawled, synced.results?.total);

  const { job_id } = await scrapix.batchScrape({
    urls: ["https://a.com", "https://b.com"],
    formats: ["markdown"],
  });
  for await (const status of scrapix.watchJob(job_id, { pollIntervalMs: 1000 })) {
    console.log(status.status, status.pages_crawled);
  }
  for await (const item of scrapix.iterJobResults(job_id, { pageSize: 50 })) {
    console.log(item.index, item.url, item.success);
  }

  const result = await scrapix.extractAndWait({
    urls: ["https://example.com/blog/*"],
    prompt: "The title and author of each post",
  });
  console.log(result.extract.data);

  try {
    await scrapix.getJob("unknown");
  } catch (error) {
    if (error instanceof NotFoundError) console.log(error.status, error.code, error.apiMessage);
  }
}
