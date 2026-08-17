import { useAccountStore } from "./account-store";

const BASE = "/api/scrapix";

export interface AuthUser {
  id: string;
  email: string;
  full_name: string | null;
  email_verified?: boolean;
  notify_job_emails?: boolean;
  account: {
    id: string;
    name: string;
    tier: string;
    active: boolean;
    role: string;
    credits_balance: number;
  } | null;
}

// Rodauth JSON responses: {success} on 2xx, {error, "field-error": [field, msg]}
// on failure. The full user object comes from /auth/me afterwards.
function rodauthError(body: Record<string, unknown>, fallback: string): string {
  const fieldError = body["field-error"] as [string, string] | undefined;
  if (fieldError) return `${fieldError[0]} ${fieldError[1]}`;
  return (body.error as string) || fallback;
}

export async function login(
  email: string,
  password: string
): Promise<AuthUser> {
  const res = await fetch(`${BASE}/auth/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify({ email, password }),
    credentials: "include",
  });
  const body = await res.json().catch(() => ({ error: "Login failed" }));
  if (!res.ok) {
    throw new Error(rodauthError(body, "Login failed"));
  }
  if (body.two_factor_required) {
    // TOTP/passkey challenge UI is a follow-up; enrollment is API-only today.
    throw new Error(
      "This account requires two-factor authentication, which the console does not support yet."
    );
  }
  return getMe();
}

export async function signup(
  email: string,
  password: string,
  full_name?: string
): Promise<AuthUser> {
  const res = await fetch(`${BASE}/auth/signup`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify({ email, password, full_name }),
    credentials: "include",
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: "Signup failed" }));
    throw new Error(rodauthError(body, "Signup failed"));
  }
  return getMe();
}

export async function logout(): Promise<void> {
  await fetch(`${BASE}/auth/logout`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: "{}",
    credentials: "include",
  });
}

export async function getMe(): Promise<AuthUser> {
  const headers: Record<string, string> = {};
  const accountId = useAccountStore.getState().selectedAccountId;
  if (accountId) {
    headers["X-Account-Id"] = accountId;
  }
  const res = await fetch(`${BASE}/auth/me`, {
    credentials: "include",
    headers,
  });
  if (!res.ok) {
    throw new Error("Not authenticated");
  }
  return res.json();
}
