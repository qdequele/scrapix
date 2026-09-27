/**
 * Engine extract contract (SCR-73): POST /extract starts a job, polled with
 * GET /extract/{id}.
 *
 * Runs against the Rust engine (CONTRACT_ENGINE_BASE_URL). Extraction needs
 * an AI provider on the engine: without one, POST /extract must fail with
 * 503 `service_unavailable` (and the rest of the scenario is skipped).
 */
import { describe, expect, it } from "vitest";

import { Session, signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { ERROR_BODY, EXTRACT_CREATED, EXTRACT_STATUS } from "../src/shapes";

const TERMINAL = ["completed", "failed", "cancelled"];

async function poll(session: Session, jobId: string) {
  for (let i = 0; i < 240; i++) {
    const res = await session.get(`/extract/${jobId}`);
    expect(res.status).toBe(200);
    assertShape(res.body, EXTRACT_STATUS);
    if (TERMINAL.includes(res.body.status)) return res.body;
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`extract ${jobId} did not finish`);
}

describe("extract contract", () => {
  it("extracts structured data, or fails clearly without an AI provider", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/extract", {
      urls: ["https://example.com/"],
      prompt: "Return the page title",
      schema: {
        type: "object",
        properties: { title: { type: "string" } },
      },
    });
    if (created.status === 503) {
      assertShape(created.body, ERROR_BODY);
      expect(created.body.code).toBe("service_unavailable");
      return;
    }
    expect(created.status).toBe(200);
    assertShape(created.body, EXTRACT_CREATED);

    const result = await poll(session, created.body.job_id);
    expect(result.status).toBe("completed");
    expect(result.data).not.toBeNull();
    expect(result.sources[0].url).toBe("https://example.com/");
  });

  it("validates the request", async () => {
    const { session } = await signupFresh();
    for (const body of [
      { urls: [], prompt: "x" },
      { urls: ["https://example.com/"] },
      { urls: ["https://*.example.com/"], prompt: "x" },
    ]) {
      const res = await session.post("/extract", body);
      // 503 when the engine has no AI provider (checked first).
      expect([400, 503]).toContain(res.status);
      assertShape(res.body, ERROR_BODY);
    }
  });

  it("GET /extract/{id} of an unknown job is 404", async () => {
    const { session } = await signupFresh();
    const res = await session.get("/extract/does-not-exist");
    expect(res.status).toBe(404);
    assertShape(res.body, ERROR_BODY);
  });
});
