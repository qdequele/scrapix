import { describe, expect, it } from "vitest";

import { Session, signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { API_KEY, CREATED_API_KEY } from "../src/shapes";

describe("api keys contract", () => {
  it("POST /account/api-keys returns the full key exactly once", async () => {
    const { session } = await signupFresh();
    const res = await session.post("/account/api-keys", { name: "ci key" });
    expect(res.status).toBe(200);
    assertShape(res.body, CREATED_API_KEY);
    // prefix is the first 12 chars of the key plus a "..." suffix
    expect(res.body.prefix.endsWith("...")).toBe(true);
    expect(res.body.key.startsWith(res.body.prefix.slice(0, -3))).toBe(true);
    expect(res.body.key.startsWith("sk_live_")).toBe(true);
  });

  it("GET /account/api-keys lists keys without exposing the secret", async () => {
    const { session } = await signupFresh();
    await session.post("/account/api-keys", { name: "listed key" });
    const res = await session.get("/account/api-keys");
    expect(res.status).toBe(200);
    assertShape(res.body, [API_KEY]);
    expect(res.body).toHaveLength(1);
  });

  it("a created key authenticates protected routes via X-API-Key", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/account/api-keys", { name: "auth key" });
    const anon = new Session();
    const res = await anon.get("/configs", { "x-api-key": created.body.key });
    expect(res.status).toBe(200);
  });

  it("PATCH /account/api-keys/{id} revokes the key", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/account/api-keys", { name: "doomed" });
    const revoked = await session.patch(
      `/account/api-keys/${created.body.id}`,
    );
    expect(revoked.status).toBe(200);

    const anon = new Session();
    const res = await anon.get("/configs", { "x-api-key": created.body.key });
    expect(res.status).toBe(401);
  });
});
