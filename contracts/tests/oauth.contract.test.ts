/**
 * OAuth 2.1 provider contract (SCR-85 phase 8).
 *
 * Frozen against the Rust implementation in bins/scrapix-api/src/auth/oauth.rs:
 * RFC 8414 metadata, RFC 7591 dynamic client registration, PKCE S256
 * authorization-code flow with a browser login form, refresh-token rotation,
 * RFC 7009 revocation, and Bearer-token access to the SaaS API.
 */
import { createHash, randomBytes } from "node:crypto";
import { describe, expect, it } from "vitest";
import { Session, signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import {
  OAUTH_CLIENT,
  OAUTH_ERROR,
  OAUTH_METADATA,
  OAUTH_TOKENS,
} from "../src/shapes";

const REDIRECT_URI = "http://localhost:9999/callback";

function pkcePair(): { verifier: string; challenge: string } {
  const verifier = randomBytes(32).toString("base64url");
  const challenge = createHash("sha256").update(verifier).digest("base64url");
  return { verifier, challenge };
}

async function registerClient(session: Session) {
  const res = await session.post("/oauth/register", {
    client_name: "Contract Test Client",
    redirect_uris: [REDIRECT_URI],
  });
  expect(res.status).toBe(200);
  assertShape(res.body, OAUTH_CLIENT);
  return res.body.client_id as string;
}

/** Run the full authorize → code → token exchange for fresh user + client. */
async function fullFlow() {
  const { session, email, password } = await signupFresh();
  const clientId = await registerClient(session);
  const { verifier, challenge } = pkcePair();

  const authRes = await session.postForm("/oauth/authorize", {
    email,
    password,
    client_id: clientId,
    redirect_uri: REDIRECT_URI,
    code_challenge: challenge,
    code_challenge_method: "S256",
    state: "xyz-state",
  });
  expect(authRes.status).toBe(307);
  const location = authRes.headers.get("location")!;
  const url = new URL(location);
  expect(`${url.origin}${url.pathname}`).toBe(REDIRECT_URI);
  expect(url.searchParams.get("state")).toBe("xyz-state");
  const code = url.searchParams.get("code")!;
  expect(code).toMatch(/^sxac_[A-Za-z0-9]{48}$/);

  const tokenRes = await session.postForm("/oauth/token", {
    grant_type: "authorization_code",
    code,
    code_verifier: verifier,
    redirect_uri: REDIRECT_URI,
    client_id: clientId,
  });
  expect(tokenRes.status).toBe(200);
  assertShape(tokenRes.body, OAUTH_TOKENS);
  return { session, email, password, clientId, code, tokens: tokenRes.body };
}

describe("GET /.well-known/oauth-authorization-server", () => {
  it("returns RFC 8414 metadata", async () => {
    const session = new Session();
    const res = await session.get("/.well-known/oauth-authorization-server");
    expect(res.status).toBe(200);
    assertShape(res.body, OAUTH_METADATA);
    expect(res.body.authorization_endpoint).toBe(
      `${res.body.issuer}/oauth/authorize`,
    );
    expect(res.body.token_endpoint).toBe(`${res.body.issuer}/oauth/token`);
    expect(res.body.registration_endpoint).toBe(
      `${res.body.issuer}/oauth/register`,
    );
    expect(res.body.revocation_endpoint).toBe(
      `${res.body.issuer}/oauth/revoke`,
    );
    expect(res.body.response_types_supported).toEqual(["code"]);
    expect(res.body.grant_types_supported).toEqual([
      "authorization_code",
      "refresh_token",
    ]);
    expect(res.body.code_challenge_methods_supported).toEqual(["S256"]);
    expect(res.body.token_endpoint_auth_methods_supported).toEqual(["none"]);
    expect(res.body.scopes_supported).toEqual(["mcp"]);
  });
});

describe("POST /oauth/register", () => {
  it("registers a client (RFC 7591)", async () => {
    const session = new Session();
    const res = await session.post("/oauth/register", {
      client_name: "Contract Test Client",
      redirect_uris: [REDIRECT_URI],
    });
    expect(res.status).toBe(200);
    assertShape(res.body, OAUTH_CLIENT);
    expect(res.body.client_id).toMatch(/^sxc_[A-Za-z0-9]{32}$/);
    expect(res.body.client_name).toBe("Contract Test Client");
    expect(res.body.redirect_uris).toEqual([REDIRECT_URI]);
  });

  it("rejects empty redirect_uris", async () => {
    const session = new Session();
    const res = await session.post("/oauth/register", { redirect_uris: [] });
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("invalid_client_metadata");
  });

  it("rejects invalid redirect_uris", async () => {
    const session = new Session();
    const res = await session.post("/oauth/register", {
      redirect_uris: ["not a url"],
    });
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("invalid_client_metadata");
  });
});

describe("GET /oauth/authorize", () => {
  it("renders the login form for a valid client", async () => {
    const session = new Session();
    const clientId = await registerClient(session);
    const { challenge } = pkcePair();
    const params = new URLSearchParams({
      client_id: clientId,
      redirect_uri: REDIRECT_URI,
      response_type: "code",
      code_challenge: challenge,
      code_challenge_method: "S256",
      state: "abc",
    });
    const res = await session.get(`/oauth/authorize?${params}`);
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("text/html");
    expect(res.body).toContain("Sign in to Scrapix");
    expect(res.body).toContain(clientId);
  });

  it("rejects unknown client_id", async () => {
    const session = new Session();
    const { challenge } = pkcePair();
    const params = new URLSearchParams({
      client_id: "sxc_doesnotexist000000000000000000000",
      redirect_uri: REDIRECT_URI,
      response_type: "code",
      code_challenge: challenge,
      code_challenge_method: "S256",
    });
    const res = await session.get(`/oauth/authorize?${params}`);
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("invalid_client");
  });

  it("rejects unregistered redirect_uri", async () => {
    const session = new Session();
    const clientId = await registerClient(session);
    const { challenge } = pkcePair();
    const params = new URLSearchParams({
      client_id: clientId,
      redirect_uri: "http://evil.example.com/steal",
      response_type: "code",
      code_challenge: challenge,
      code_challenge_method: "S256",
    });
    const res = await session.get(`/oauth/authorize?${params}`);
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("invalid_request");
  });

  it("rejects non-code response_type", async () => {
    const session = new Session();
    const clientId = await registerClient(session);
    const { challenge } = pkcePair();
    const params = new URLSearchParams({
      client_id: clientId,
      redirect_uri: REDIRECT_URI,
      response_type: "token",
      code_challenge: challenge,
      code_challenge_method: "S256",
    });
    const res = await session.get(`/oauth/authorize?${params}`);
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("unsupported_response_type");
  });

  it("rejects plain code_challenge_method", async () => {
    const session = new Session();
    const clientId = await registerClient(session);
    const { challenge } = pkcePair();
    const params = new URLSearchParams({
      client_id: clientId,
      redirect_uri: REDIRECT_URI,
      response_type: "code",
      code_challenge: challenge,
      code_challenge_method: "plain",
    });
    const res = await session.get(`/oauth/authorize?${params}`);
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("invalid_request");
  });
});

describe("POST /oauth/authorize", () => {
  it("rejects invalid credentials with an HTML error page", async () => {
    const { session, email } = await signupFresh();
    const clientId = await registerClient(session);
    const { challenge } = pkcePair();
    const res = await session.postForm("/oauth/authorize", {
      email,
      password: "wrong-password",
      client_id: clientId,
      redirect_uri: REDIRECT_URI,
      code_challenge: challenge,
      code_challenge_method: "S256",
      state: "",
    });
    expect(res.status).toBe(400);
    expect(res.headers.get("content-type")).toContain("text/html");
    expect(res.body).toContain("Invalid email or password");
  });
});

describe("POST /oauth/token", () => {
  it("exchanges a code for tokens and the Bearer token works", async () => {
    const { tokens } = await fullFlow();
    expect(tokens.access_token).toMatch(/^sxat_[A-Za-z0-9]{48}$/);
    expect(tokens.refresh_token).toMatch(/^sxrt_[A-Za-z0-9]{48}$/);
    expect(tokens.token_type).toBe("Bearer");
    expect(tokens.expires_in).toBe(3600);
    expect(tokens.scope).toBe("mcp");

    // The access token authenticates API-key-or-Bearer routes (e.g. configs).
    // NOTE: /account/* is session-cookie-only in the frozen contract.
    const apiSession = new Session();
    const configsRes = await apiSession.get("/configs", {
      authorization: `Bearer ${tokens.access_token}`,
    });
    expect(configsRes.status).toBe(200);
    expect(Array.isArray(configsRes.body)).toBe(true);
  });

  it("rejects a reused authorization code", async () => {
    const { session, code, tokens: _tokens, clientId } = await fullFlow();
    const { verifier } = pkcePair();
    const res = await session.postForm("/oauth/token", {
      grant_type: "authorization_code",
      code,
      code_verifier: verifier,
      redirect_uri: REDIRECT_URI,
      client_id: clientId,
    });
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("invalid_grant");
    expect(res.body.error_description).toBe("Authorization code already used");
  });

  it("rejects a bad PKCE verifier", async () => {
    const { session, email, password } = await signupFresh();
    const clientId = await registerClient(session);
    const { challenge } = pkcePair();
    const authRes = await session.postForm("/oauth/authorize", {
      email,
      password,
      client_id: clientId,
      redirect_uri: REDIRECT_URI,
      code_challenge: challenge,
      code_challenge_method: "S256",
      state: "",
    });
    expect(authRes.status).toBe(307);
    const code = new URL(authRes.headers.get("location")!).searchParams.get(
      "code",
    )!;

    const res = await session.postForm("/oauth/token", {
      grant_type: "authorization_code",
      code,
      code_verifier: "wrong-verifier-wrong-verifier-wrong-verifier",
      redirect_uri: REDIRECT_URI,
      client_id: clientId,
    });
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("invalid_grant");
    expect(res.body.error_description).toBe("PKCE verification failed");
  });

  it("rejects unsupported grant types", async () => {
    const session = new Session();
    const res = await session.postForm("/oauth/token", {
      grant_type: "client_credentials",
    });
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("unsupported_grant_type");
  });

  it("refreshes tokens with rotation", async () => {
    const { session, tokens } = await fullFlow();

    const refreshRes = await session.postForm("/oauth/token", {
      grant_type: "refresh_token",
      refresh_token: tokens.refresh_token,
    });
    expect(refreshRes.status).toBe(200);
    assertShape(refreshRes.body, OAUTH_TOKENS);
    expect(refreshRes.body.access_token).not.toBe(tokens.access_token);
    expect(refreshRes.body.refresh_token).not.toBe(tokens.refresh_token);

    // Rotation: the old refresh token is revoked.
    const reuse = await session.postForm("/oauth/token", {
      grant_type: "refresh_token",
      refresh_token: tokens.refresh_token,
    });
    expect(reuse.status).toBe(400);
    assertShape(reuse.body, OAUTH_ERROR);
    expect(reuse.body.error).toBe("invalid_grant");
    expect(reuse.body.error_description).toBe(
      "Refresh token has been revoked",
    );
  });

  it("rejects an invalid refresh token", async () => {
    const session = new Session();
    const res = await session.postForm("/oauth/token", {
      grant_type: "refresh_token",
      refresh_token: "sxrt_nope",
    });
    expect(res.status).toBe(400);
    assertShape(res.body, OAUTH_ERROR);
    expect(res.body.error).toBe("invalid_grant");
  });
});

describe("POST /oauth/revoke", () => {
  it("revokes an access token (RFC 7009)", async () => {
    const { session, tokens } = await fullFlow();
    const res = await session.postForm("/oauth/revoke", {
      token: tokens.access_token,
    });
    expect(res.status).toBe(200);

    const apiSession = new Session();
    const configsRes = await apiSession.get("/configs", {
      authorization: `Bearer ${tokens.access_token}`,
    });
    expect(configsRes.status).toBe(401);
    expect(configsRes.body.code).toBe("invalid_bearer_token");
  });

  it("returns 200 for unknown tokens", async () => {
    const session = new Session();
    const res = await session.postForm("/oauth/revoke", {
      token: "sxat_doesnotexist",
    });
    expect(res.status).toBe(200);
  });
});
