/**
 * HTTP client for contract tests. Points at whichever backend is under test
 * via CONTRACT_BASE_URL (default: the local Rust API).
 *
 * Sessions are the `scrapix_session` HS256 JWT cookie set by signup/login;
 * the client captures Set-Cookie and replays it, mimicking the console.
 */

export const BASE_URL =
  process.env.CONTRACT_BASE_URL ?? "http://localhost:8080";

/**
 * During the migration, route groups may live on different backends
 * (mirroring the edge proxy's path routing). CONTRACT_AUTH_BASE_URL points at
 * the backend serving /auth; CONTRACT_ACCOUNT_BASE_URL at the one serving
 * /account and /webhooks. Both default to CONTRACT_BASE_URL.
 */
export const AUTH_BASE_URL =
  process.env.CONTRACT_AUTH_BASE_URL ?? BASE_URL;
export const ACCOUNT_BASE_URL =
  process.env.CONTRACT_ACCOUNT_BASE_URL ?? BASE_URL;

function baseFor(path: string): string {
  if (path.startsWith("/auth")) return AUTH_BASE_URL;
  if (path.startsWith("/account") || path.startsWith("/webhooks")) {
    return ACCOUNT_BASE_URL;
  }
  return BASE_URL;
}

export interface ApiResponse {
  status: number;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  body: any;
  headers: Headers;
}

export class Session {
  private cookie: string | null = null;

  async request(
    method: string,
    path: string,
    body?: unknown,
    extraHeaders?: Record<string, string>,
  ): Promise<ApiResponse> {
    const headers: Record<string, string> = { ...extraHeaders };
    if (body !== undefined) headers["content-type"] = "application/json";
    if (this.cookie) headers["cookie"] = this.cookie;

    const res = await fetch(`${baseFor(path)}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      redirect: "manual",
    });

    const setCookie = res.headers.get("set-cookie");
    if (setCookie?.includes("scrapix_session=")) {
      const match = setCookie.match(/scrapix_session=([^;]*)/);
      // An empty value is the logout-clearing cookie.
      this.cookie = match && match[1] ? `scrapix_session=${match[1]}` : null;
    }

    const text = await res.text();
    let parsed: unknown = null;
    try {
      parsed = text ? JSON.parse(text) : null;
    } catch {
      parsed = text;
    }
    return { status: res.status, body: parsed, headers: res.headers };
  }

  get(path: string, headers?: Record<string, string>) {
    return this.request("GET", path, undefined, headers);
  }
  post(path: string, body?: unknown, headers?: Record<string, string>) {
    return this.request("POST", path, body, headers);
  }
  patch(path: string, body?: unknown, headers?: Record<string, string>) {
    return this.request("PATCH", path, body, headers);
  }
  delete(path: string, headers?: Record<string, string>) {
    return this.request("DELETE", path, undefined, headers);
  }

  hasSession(): boolean {
    return this.cookie !== null;
  }
}

let counter = 0;

/** Create a fresh user + account and return an authenticated session. */
export async function signupFresh(): Promise<{
  session: Session;
  email: string;
  password: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  user: any;
}> {
  const session = new Session();
  const email = `contract-${Date.now()}-${counter++}@example.com`;
  const password = "contract-test-password-123";
  const res = await session.post("/auth/signup", {
    email,
    password,
    full_name: "Contract Test",
  });
  if (res.status !== 200 && res.status !== 201) {
    throw new Error(
      `signup failed (${res.status}): ${JSON.stringify(res.body)} — is the backend running at ${BASE_URL} with auth enabled?`,
    );
  }
  return { session, email, password, user: res.body };
}
