/**
 * Engine batch scrape contract (SCR-74): POST /batch/scrape starts a job
 * whose per-URL results are served by GET /job/{id}/results.
 *
 * Runs against the Rust engine (CONTRACT_ENGINE_BASE_URL). Unlike /crawl it
 * does not need the Kafka pipeline: the engine scrapes the URLs itself. The
 * target URLs are public (example.com), plus one invalid URL that must come
 * back as a failed item without failing the batch.
 */
import { describe, expect, it } from "vitest";

import { Session, signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import {
  BATCH_SCRAPE_CREATED,
  ERROR_BODY,
  JOB_RESULTS,
  JOB_STATUS,
} from "../src/shapes";

const TERMINAL = ["completed", "failed", "cancelled"];

async function waitTerminal(session: Session, jobId: string) {
  for (let i = 0; i < 120; i++) {
    const res = await session.get(`/job/${jobId}/status`);
    expect(res.status).toBe(200);
    if (TERMINAL.includes(res.body.status)) return res.body;
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`batch ${jobId} did not finish`);
}

describe("batch scrape contract", () => {
  it("scrapes every URL and reports failures as items", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/batch/scrape", {
      urls: ["https://example.com/", "not a url"],
      formats: ["markdown"],
    });
    expect(created.status).toBe(200);
    assertShape(created.body, BATCH_SCRAPE_CREATED);
    expect(created.body.urls_count).toBe(2);

    const status = await waitTerminal(session, created.body.job_id);
    assertShape(status, JOB_STATUS);
    expect(status.job_type).toBe("batch_scrape");
    expect(status.status).toBe("completed");

    const results = await session.get(
      `/job/${created.body.job_id}/results?limit=10`,
    );
    expect(results.status).toBe(200);
    assertShape(results.body, JOB_RESULTS);
    expect(results.body.job_type).toBe("batch_scrape");
    expect(results.body.total).toBe(2);
    expect(results.body.next).toBeNull();
    const failed = results.body.data.find(
      (item: { success: boolean }) => !item.success,
    );
    expect(failed.source_url).toBe("not a url");
    expect(failed.error.code).toBe("validation_error");
  });

  it("rejects an empty or oversized batch", async () => {
    const { session } = await signupFresh();
    for (const urls of [
      [],
      Array.from({ length: 1001 }, (_, i) => `https://example.com/${i}`),
    ]) {
      const res = await session.post("/batch/scrape", { urls });
      expect(res.status).toBe(400);
      assertShape(res.body, ERROR_BODY);
    }
  });
});
