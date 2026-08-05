import { describe, expect, it } from "vitest";

import { Session, signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { ERROR_BODY, MESSAGE, USER } from "../src/shapes";

describe("auth contract", () => {
  it("POST /auth/signup returns the user with account and sets the session cookie", async () => {
    const { session, user, email } = await signupFresh();
    assertShape(user, USER);
    expect(user.email).toBe(email);
    expect(user.email_verified).toBe(false);
    expect(session.hasSession()).toBe(true);
  });

  it("POST /auth/signup rejects passwords shorter than 12 chars with an error body", async () => {
    const session = new Session();
    const res = await session.post("/auth/signup", {
      email: `short-pw-${Date.now()}@example.com`,
      password: "short",
    });
    expect(res.status).toBe(400);
    assertShape(res.body, ERROR_BODY);
  });

  it("POST /auth/login authenticates and returns the same user shape", async () => {
    const { email, password } = await signupFresh();
    const session = new Session();
    const res = await session.post("/auth/login", { email, password });
    expect(res.status).toBe(200);
    assertShape(res.body, USER);
    expect(session.hasSession()).toBe(true);
  });

  it("POST /auth/login rejects a wrong password with 401", async () => {
    const { email } = await signupFresh();
    const session = new Session();
    const res = await session.post("/auth/login", {
      email,
      password: "definitely-not-the-password",
    });
    expect(res.status).toBe(401);
    assertShape(res.body, ERROR_BODY);
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

  it("POST /auth/logout clears the session", async () => {
    const { session } = await signupFresh();
    const res = await session.post("/auth/logout");
    expect(res.status).toBe(200);
    assertShape(res.body, MESSAGE);
    expect(session.hasSession()).toBe(false);
    const me = await session.get("/auth/me");
    expect(me.status).toBe(401);
  });
});
