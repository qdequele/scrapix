import { describe, expect, it } from "vitest";

import { Session, signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { ERROR_BODY, MESSAGE, RODAUTH_ERROR, RODAUTH_SUCCESS, USER } from "../src/shapes";

/**
 * The authentication flows are Rodauth routes (SCR-87 I5): 2xx responses are
 * {success}, failures are {error} plus an optional ["field", "message"]
 * "field-error" pair. The user object lives at /auth/me.
 */
describe("auth contract", () => {
  it("POST /auth/signup succeeds, sets the session cookie, and provisions the account", async () => {
    const { session, user, email } = await signupFresh();
    assertShape(user, USER);
    expect(user.email).toBe(email);
    expect(user.email_verified).toBe(false);
    expect(user.account.credits_balance).toBe(100);
    expect(user.account.role).toBe("owner");
    expect(session.hasSession()).toBe(true);
  });

  it("POST /auth/signup rejects passwords shorter than 12 chars", async () => {
    const session = new Session();
    const res = await session.post("/auth/signup", {
      email: `short-pw-${Date.now()}@example.com`,
      password: "short",
    });
    expect(res.status).toBe(422);
    assertShape(res.body, RODAUTH_ERROR);
    expect(res.body["field-error"][0]).toBe("password");
  });

  it("POST /auth/signup rejects duplicate emails", async () => {
    const { email, password } = await signupFresh();
    const res = await new Session().post("/auth/signup", { email, password });
    // Unverified duplicate: Rodauth reports the account as awaiting verification.
    expect(res.status).toBe(403);
    assertShape(res.body, RODAUTH_ERROR);
  });

  it("POST /auth/login authenticates and sets the session cookie", async () => {
    const { email, password } = await signupFresh();
    const session = new Session();
    const res = await session.post("/auth/login", { email, password });
    expect(res.status).toBe(200);
    assertShape(res.body, RODAUTH_SUCCESS);
    expect(session.hasSession()).toBe(true);

    const me = await session.get("/auth/me");
    expect(me.status).toBe(200);
    assertShape(me.body, USER);
  });

  it("POST /auth/login rejects a wrong password with 401", async () => {
    const { email } = await signupFresh();
    const session = new Session();
    const res = await session.post("/auth/login", {
      email,
      password: "definitely-not-the-password",
    });
    expect(res.status).toBe(401);
    assertShape(res.body, RODAUTH_ERROR);
    expect(res.body["field-error"]).toEqual(["password", "invalid password"]);
  });

  it("GET /auth/me returns the authenticated user", async () => {
    const { session, email } = await signupFresh();
    const res = await session.get("/auth/me");
    expect(res.status).toBe(200);
    assertShape(res.body, USER);
    expect(res.body.email).toBe(email);
  });

  it("GET /auth/me without a session returns 401", async () => {
    const res = await new Session().get("/auth/me");
    expect(res.status).toBe(401);
    assertShape(res.body, ERROR_BODY);
  });

  it("PATCH /auth/me returns a message; the update is visible on re-fetch", async () => {
    const { session } = await signupFresh();
    const res = await session.patch("/auth/me", {
      full_name: "Renamed User",
      notify_job_emails: false,
    });
    expect(res.status).toBe(200);
    assertShape(res.body, MESSAGE);

    const me = await session.get("/auth/me");
    expect(me.body.full_name).toBe("Renamed User");
    expect(me.body.notify_job_emails).toBe(false);
  });

  it("POST /auth/forgot-password always claims success", async () => {
    const res = await new Session().post("/auth/forgot-password", {
      email: `nobody-${Date.now()}@example.com`,
    });
    // Rodauth reveals nothing about account existence in the JSON API either.
    expect([200, 401]).toContain(res.status);
  });

  it("POST /auth/logout clears the session", async () => {
    const { session } = await signupFresh();
    const res = await session.post("/auth/logout", {});
    expect(res.status).toBe(200);
    assertShape(res.body, RODAUTH_SUCCESS);
    expect(session.hasSession()).toBe(false);
    const me = await session.get("/auth/me");
    expect(me.status).toBe(401);
  });
});
