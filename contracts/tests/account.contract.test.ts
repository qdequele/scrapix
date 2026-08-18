import { describe, expect, it } from "vitest";

import { signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { ACCOUNT, INVITE, MEMBER, MESSAGE } from "../src/shapes";

describe("account contract", () => {
  it("GET /account returns the account with the caller's role", async () => {
    const { session } = await signupFresh();
    const res = await session.get("/account");
    expect(res.status).toBe(200);
    assertShape(res.body, ACCOUNT);
    expect(res.body.role).toBe("owner");
  });

  it("PATCH /account returns a message; the rename is visible on re-fetch", async () => {
    const { session } = await signupFresh();
    const res = await session.patch("/account", { name: "Renamed Account" });
    expect(res.status).toBe(200);
    assertShape(res.body, MESSAGE);

    const account = await session.get("/account");
    expect(account.body.name).toBe("Renamed Account");
  });

  it("GET /account/members lists the owner as sole member", async () => {
    const { session, email } = await signupFresh();
    const res = await session.get("/account/members");
    expect(res.status).toBe(200);
    assertShape(res.body, [MEMBER]);
    expect(res.body).toHaveLength(1);
    expect(res.body[0].email).toBe(email);
    expect(res.body[0].role).toBe("owner");
  });

  it("POST /account/members/invite creates a pending invite, listed by GET /account/invites", async () => {
    const { session } = await signupFresh();
    const invitee = `invitee-${Date.now()}@example.com`;
    const created = await session.post("/account/members/invite", {
      email: invitee,
      role: "member",
    });
    expect(created.status).toBe(200);
    assertShape(created.body, INVITE);
    expect(created.body.status).toBe("pending");

    const list = await session.get("/account/invites");
    expect(list.status).toBe(200);
    assertShape(list.body, [INVITE]);
    expect(list.body.some((i: { email: string }) => i.email === invitee)).toBe(
      true,
    );
  });

  it("DELETE /account/invites/{id} revokes a pending invite", async () => {
    const { session } = await signupFresh();
    const created = await session.post("/account/members/invite", {
      email: `revoked-${Date.now()}@example.com`,
      role: "member",
    });
    expect(created.status).toBe(200);
    const res = await session.delete(`/account/invites/${created.body.id}`);
    expect(res.status).toBe(200);
  });
});
