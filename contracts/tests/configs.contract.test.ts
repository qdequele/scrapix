import { describe, expect, it } from "vitest";

import { signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { SAVED_CONFIG } from "../src/shapes";

const SAMPLE_CONFIG = {
  start_urls: ["https://example.com"],
  max_pages: 10,
  index_uid: "contract-test",
};

describe("saved configs contract", () => {
  it("POST /configs creates a saved config", async () => {
    const { session } = await signupFresh();
    const res = await session.post("/configs", {
      name: "contract config",
      description: "created by contract tests",
      config: SAMPLE_CONFIG,
    });
    expect(res.status).toBe(201);
    assertShape(res.body, SAVED_CONFIG);
    expect(res.body.name).toBe("contract config");
    expect(res.body.cron_enabled).toBe(false);
  });

  it("GET /configs lists the account's configs", async () => {
    const { session } = await signupFresh();
    await session.post("/configs", { name: "listed", config: SAMPLE_CONFIG });
    const res = await session.get("/configs");
    expect(res.status).toBe(200);
    assertShape(res.body, [SAVED_CONFIG]);
    expect(res.body).toHaveLength(1);
  });

  it("GET /configs/{id} fetches one config", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/configs", {
      name: "fetched",
      config: SAMPLE_CONFIG,
    });
    const res = await session.get(`/configs/${created.body.id}`);
    expect(res.status).toBe(200);
    assertShape(res.body, SAVED_CONFIG);
    expect(res.body.id).toBe(created.body.id);
  });

  it("PATCH /configs/{id} updates name and cron settings", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/configs", {
      name: "before",
      config: SAMPLE_CONFIG,
    });
    const res = await session.patch(`/configs/${created.body.id}`, {
      name: "after",
      cron_expression: "0 0 * * *",
      cron_enabled: true,
    });
    expect(res.status).toBe(200);
    assertShape(res.body, SAVED_CONFIG);
    expect(res.body.name).toBe("after");
    expect(res.body.cron_enabled).toBe(true);
    expect(res.body.cron_expression).toBe("0 0 * * *");
  });

  it("DELETE /configs/{id} removes the config", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/configs", {
      name: "doomed",
      config: SAMPLE_CONFIG,
    });
    const del = await session.delete(`/configs/${created.body.id}`);
    expect(del.status).toBe(204);
    const gone = await session.get(`/configs/${created.body.id}`);
    expect(gone.status).toBe(404);
  });

  it("configs are scoped to the account", async () => {
    const alice = await signupFresh();
    const bob = await signupFresh();
    const created = await alice.session.post("/configs", {
      name: "private",
      config: SAMPLE_CONFIG,
    });
    const res = await bob.session.get(`/configs/${created.body.id}`);
    expect(res.status).toBe(404);
  });
});
