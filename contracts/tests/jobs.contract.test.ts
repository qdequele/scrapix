/**
 * Engine job control contract (R5): cancel, pause and resume.
 *
 * These routes live on the Rust engine (CONTRACT_ENGINE_BASE_URL, default
 * :8080), which validates the Rails session cookie. The engine needs its
 * pipeline (Kafka + frontier) for the crawl to be accepted.
 *
 * Transitions: pause Running → Paused, resume Paused → Running, cancel any
 * non-terminal status → Cancelled. Anything else is 409 `conflict` and the
 * status is unchanged.
 *
 * Determinism: the crawl targets a multi-page site with a page budget, so it
 * normally outlives the test, and every step that can race the job's own
 * completion accepts a 409 only when the job is (already) terminal — in
 * which case the rest of that scenario is skipped, never failed.
 */
import { describe, expect, it } from "vitest";

import { ApiResponse, Session, signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { ERROR_BODY, JOB_STATUS } from "../src/shapes";

const TERMINAL = ["completed", "failed", "cancelled"];

async function startCrawl(session: Session): Promise<string> {
  const res = await session.post("/crawl", {
    start_urls: ["https://www.meilisearch.com/docs"],
    index_uid: `contract-jobs-${Date.now()}`,
    max_pages: 200,
  });
  expect(res.status).toBe(200);
  expect(typeof res.body.job_id).toBe("string");
  return res.body.job_id;
}

async function statusOf(session: Session, jobId: string): Promise<string> {
  const res = await session.get(`/job/${jobId}/status`);
  expect(res.status).toBe(200);
  return res.body.status;
}

function expectConflict(res: ApiResponse) {
  expect(res.status).toBe(409);
  assertShape(res.body, ERROR_BODY);
  expect(res.body.code).toBe("conflict");
}

/**
 * `res` is 200 with `expected` status, or a 409 because the job finished on
 * its own first. Returns false in the latter case (skip the rest).
 */
async function okOrTerminal(
  session: Session,
  jobId: string,
  res: ApiResponse,
  expected: string,
): Promise<boolean> {
  if (res.status === 409) {
    expectConflict(res);
    expect(TERMINAL).toContain(await statusOf(session, jobId));
    return false;
  }
  expect(res.status).toBe(200);
  assertShape(res.body, JOB_STATUS);
  expect(res.body.status).toBe(expected);
  return true;
}

describe("jobs contract", () => {
  it("POST /job/{id}/pause and /resume flip Running <-> Paused", async () => {
    const { session } = await signupFresh();
    const jobId = await startCrawl(session);

    const paused = await session.post(`/job/${jobId}/pause`);
    if (!(await okOrTerminal(session, jobId, paused, "paused"))) return;
    // Paused is stable: it never completes on its own.
    expectConflict(await session.post(`/job/${jobId}/pause`));
    expect(await statusOf(session, jobId)).toBe("paused");

    const resumed = await session.post(`/job/${jobId}/resume`);
    expect(resumed.status).toBe(200);
    assertShape(resumed.body, JOB_STATUS);
    expect(resumed.body.status).toBe("running");

    const again = await session.post(`/job/${jobId}/resume`);
    expectConflict(again); // running or, if it already finished, terminal

    await session.delete(`/job/${jobId}`);
  });

  it("DELETE /job/{id} cancels once; later controls are 409", async () => {
    const { session } = await signupFresh();
    const jobId = await startCrawl(session);

    const cancelled = await session.delete(`/job/${jobId}`);
    if (!(await okOrTerminal(session, jobId, cancelled, "cancelled"))) return;

    expectConflict(await session.delete(`/job/${jobId}`));
    expectConflict(await session.post(`/job/${jobId}/pause`));
    expectConflict(await session.post(`/job/${jobId}/resume`));
    expect(await statusOf(session, jobId)).toBe("cancelled");
  });

  it("a paused job can be cancelled", async () => {
    const { session } = await signupFresh();
    const jobId = await startCrawl(session);
    const paused = await session.post(`/job/${jobId}/pause`);
    if (!(await okOrTerminal(session, jobId, paused, "paused"))) return;
    // A paused job cannot finish on its own: this cancel is deterministic.
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
