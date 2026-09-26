/**
 * Engine job control contract (R5): cancel, pause and resume.
 *
 * These routes live on the Rust engine (CONTRACT_ENGINE_BASE_URL, default
 * :8080), which validates the Rails session cookie. The engine needs its
 * pipeline (Kafka + frontier) for the crawl to be accepted; the jobs are
 * cancelled at the end, so they never crawl more than a page or two.
 *
 * Transitions: pause Running → Paused, resume Paused → Running, cancel any
 * non-terminal status → Cancelled. Anything else is 409 `conflict` and the
 * status is unchanged.
 */
import { describe, expect, it } from "vitest";

import { Session, signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { ERROR_BODY, JOB_STATUS } from "../src/shapes";

async function startCrawl(session: Session): Promise<string> {
  const res = await session.post("/crawl", {
    start_urls: ["https://example.com"],
    index_uid: `contract-jobs-${Date.now()}`,
    max_pages: 5,
  });
  expect(res.status).toBe(200);
  expect(typeof res.body.job_id).toBe("string");
  return res.body.job_id;
}

function expectConflict(res: { status: number; body: unknown }) {
  expect(res.status).toBe(409);
  assertShape(res.body, ERROR_BODY);
  expect((res.body as { code: string }).code).toBe("conflict");
}

describe("jobs contract", () => {
  it("POST /job/{id}/pause and /resume flip Running <-> Paused", async () => {
    const { session } = await signupFresh();
    const jobId = await startCrawl(session);

    const paused = await session.post(`/job/${jobId}/pause`);
    expect(paused.status).toBe(200);
    assertShape(paused.body, JOB_STATUS);
    expect(paused.body.status).toBe("paused");
    expectConflict(await session.post(`/job/${jobId}/pause`));

    const status = await session.get(`/job/${jobId}/status`);
    expect(status.body.status).toBe("paused");

    const resumed = await session.post(`/job/${jobId}/resume`);
    expect(resumed.status).toBe(200);
    assertShape(resumed.body, JOB_STATUS);
    expect(resumed.body.status).toBe("running");
    expectConflict(await session.post(`/job/${jobId}/resume`));

    await session.delete(`/job/${jobId}`);
  });

  it("DELETE /job/{id} cancels once; later controls are 409", async () => {
    const { session } = await signupFresh();
    const jobId = await startCrawl(session);

    const cancelled = await session.delete(`/job/${jobId}`);
    expect(cancelled.status).toBe(200);
    assertShape(cancelled.body, JOB_STATUS);
    expect(cancelled.body.status).toBe("cancelled");

    expectConflict(await session.delete(`/job/${jobId}`));
    expectConflict(await session.post(`/job/${jobId}/pause`));
    expectConflict(await session.post(`/job/${jobId}/resume`));
    const status = await session.get(`/job/${jobId}/status`);
    expect(status.body.status).toBe("cancelled");
  });

  it("a paused job can be cancelled", async () => {
    const { session } = await signupFresh();
    const jobId = await startCrawl(session);
    expect((await session.post(`/job/${jobId}/pause`)).status).toBe(200);
    const cancelled = await session.delete(`/job/${jobId}`);
    expect(cancelled.status).toBe(200);
    expect(cancelled.body.status).toBe("cancelled");
  });

  it("pause/resume of an unknown or foreign job is 404", async () => {
    const owner = await signupFresh();
    const other = await signupFresh();
    const jobId = await startCrawl(owner.session);

    for (const path of [
      "/job/does-not-exist/pause",
      "/job/does-not-exist/resume",
      `/job/${jobId}/pause`,
      `/job/${jobId}/resume`,
    ]) {
      const res = await other.session.post(path);
      expect(res.status).toBe(404);
      assertShape(res.body, ERROR_BODY);
    }

    await owner.session.delete(`/job/${jobId}`);
  });
});
