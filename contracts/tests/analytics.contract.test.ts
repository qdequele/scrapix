import { describe, expect, it } from "vitest";

import { Session } from "../src/client";
import { assertShape } from "../src/shape";
import { ANALYTICS_RESPONSE, PIPE_INFO } from "../src/shapes";

/**
 * Analytics pipes require ClickHouse. When the backend runs without it the
 * routes are absent (404) — the suite records that and skips shape checks so
 * contract runs stay green on minimal stacks.
 */
describe("analytics contract", () => {
  const session = new Session();

  it("GET /analytics/v0/pipes lists available pipes (or analytics is disabled)", async () => {
    const res = await session.get("/analytics/v0/pipes");
    if (res.status === 404) {
      console.warn("analytics disabled (no ClickHouse) — skipping pipe checks");
      return;
    }
    expect(res.status).toBe(200);
    assertShape(res.body, [PIPE_INFO]);
  });

  for (const pipe of [
    "kpis.json?hours=24",
    "top_domains.json?hours=24&limit=5",
    "hourly_stats.json?hours=24",
    "error_distribution.json?hours=24",
  ]) {
    it(`GET /analytics/v0/pipes/${pipe} returns the Tinybird envelope`, async () => {
      const res = await session.get(`/analytics/v0/pipes/${pipe}`);
      if (res.status === 404) return; // analytics disabled
      expect(res.status).toBe(200);
      assertShape(res.body, ANALYTICS_RESPONSE);
    });
  }
});
