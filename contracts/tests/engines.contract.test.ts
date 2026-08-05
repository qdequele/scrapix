import { describe, expect, it } from "vitest";

import { signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { ENGINE } from "../src/shapes";

describe("engines contract", () => {
  it("POST /engines registers a Meilisearch engine", async () => {
    const { session } = await signupFresh();
    const res = await session.post("/engines", {
      name: "contract engine",
      url: "http://localhost:7700",
      api_key: "masterKey",
    });
    expect(res.status).toBe(201);
    assertShape(res.body, ENGINE);
    expect(res.body.name).toBe("contract engine");
  });

  it("GET /engines lists the account's engines", async () => {
    const { session } = await signupFresh();
    await session.post("/engines", {
      name: "listed",
      url: "http://localhost:7700",
    });
    const res = await session.get("/engines");
    expect(res.status).toBe(200);
    assertShape(res.body, [ENGINE]);
  });

  it("PATCH /engines/{id} updates the engine", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/engines", {
      name: "before",
      url: "http://localhost:7700",
    });
    const res = await session.patch(`/engines/${created.body.id}`, {
      name: "after",
    });
    expect(res.status).toBe(200);
    assertShape(res.body, ENGINE);
    expect(res.body.name).toBe("after");
  });

  it("POST /engines/{id}/default marks one engine as default", async () => {
    const { session } = await signupFresh();
    const first = await session.post("/engines", {
      name: "first",
      url: "http://localhost:7700",
    });
    const second = await session.post("/engines", {
      name: "second",
      url: "http://localhost:7701",
    });
    const res = await session.post(`/engines/${second.body.id}/default`);
    expect(res.status).toBe(200);

    const list = await session.get("/engines");
    const byId = Object.fromEntries(
      list.body.map((e: { id: string; is_default: boolean }) => [
        e.id,
        e.is_default,
      ]),
    );
    expect(byId[second.body.id]).toBe(true);
    expect(byId[first.body.id]).toBe(false);
  });

  it("DELETE /engines/{id} removes the engine", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/engines", {
      name: "doomed",
      url: "http://localhost:7700",
    });
    const del = await session.delete(`/engines/${created.body.id}`);
    expect(del.status).toBe(204);
    const gone = await session.get(`/engines/${created.body.id}`);
    expect(gone.status).toBe(404);
  });
});
