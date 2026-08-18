import { useAccountStore } from "./account-store";

const BASE = "/api/scrapix";

export interface AuthUser {
  id: string;
  email: string;
  full_name: string | null;
  email_verified?: boolean;
  notify_job_emails?: boolean;
  mfa?: { totp: boolean; passkeys: number };
  account: {
    id: string;
    name: string;
    tier: string;
    active: boolean;
    role: string;
    credits_balance: number;
  } | null;
}

export type LoginResult =
  | { twoFactorRequired: true }
  | { twoFactorRequired: false; user: AuthUser };

// Rodauth JSON envelope: {success} on 2xx, {error, "field-error": [field, msg]}
// on failure. The full user object comes from /auth/me afterwards.
function rodauthError(body: Record<string, unknown>, fallback: string): string {
  const fieldError = body["field-error"] as [string, string] | undefined;
  if (fieldError) return `${fieldError[0]} ${fieldError[1]}`;
  return (body.error as string) || fallback;
}

async function rodauthPost(
  path: string,
  body: Record<string, unknown>
): Promise<Record<string, unknown>> {
  const res = await fetch(`${BASE}${path}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(body),
    credentials: "include",
  });
  const parsed = await res.json().catch(() => ({}));
  return { __status: res.status, ...parsed };
}

function ok(body: Record<string, unknown>): boolean {
  return (body.__status as number) < 400;
}

export async function login(
  email: string,
  password: string
): Promise<LoginResult> {
  const body = await rodauthPost("/auth/login", { email, password });
  if (!ok(body)) throw new Error(rodauthError(body, "Login failed"));
  if (body.two_factor_required) return { twoFactorRequired: true };
  return { twoFactorRequired: false, user: await getMe() };
}

export async function signup(
  email: string,
  password: string,
  full_name?: string
): Promise<AuthUser> {
  const body = await rodauthPost("/auth/signup", { email, password, full_name });
  if (!ok(body)) throw new Error(rodauthError(body, "Signup failed"));
  return getMe();
}

export async function logout(): Promise<void> {
  await rodauthPost("/auth/logout", {});
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

// ── Second factor at login ──────────────────────────────────────────────────

export async function otpAuth(otp: string): Promise<AuthUser> {
  const body = await rodauthPost("/auth/otp-auth", { otp });
  if (!ok(body)) throw new Error(rodauthError(body, "Invalid authentication code"));
  return getMe();
}

export async function recoveryAuth(code: string): Promise<AuthUser> {
  const body = await rodauthPost("/auth/recovery-auth", { "recovery-code": code });
  if (!ok(body)) throw new Error(rodauthError(body, "Invalid recovery code"));
  return getMe();
}

// ── Email verification & password reset (token pages) ───────────────────────

export async function verifyEmail(key: string): Promise<string> {
  const body = await rodauthPost("/auth/verify-email", { key });
  if (!ok(body)) throw new Error(rodauthError(body, "Invalid or expired verification link"));
  return body.success as string;
}

export async function resendVerification(): Promise<string> {
  const body = await rodauthPost("/auth/resend-verification", {});
  if (!ok(body)) throw new Error(rodauthError(body, "Could not resend the verification email"));
  return body.success as string;
}

export async function forgotPassword(email: string): Promise<void> {
  await rodauthPost("/auth/forgot-password", { email });
  // Always generic — the endpoint reveals nothing about account existence.
}

export async function resetPassword(key: string, password: string): Promise<string> {
  const body = await rodauthPost("/auth/reset-password", { key, password });
  if (!ok(body)) throw new Error(rodauthError(body, "Invalid or expired reset link"));
  return body.success as string;
}

// ── TOTP enrollment (settings) ───────────────────────────────────────────────

export interface OtpSetupStart {
  secret: string;
  rawSecret: string;
  provisioningUri: string;
}

export async function otpSetupStart(
  password: string,
  email: string
): Promise<OtpSetupStart> {
  // The first POST intentionally "fails" and returns the provisioning secrets.
  const body = await rodauthPost("/auth/otp-setup", { password });
  const secret = body.otp_secret as string | undefined;
  const rawSecret = body.otp_raw_secret as string | undefined;
  if (!secret) throw new Error(rodauthError(body, "Could not start TOTP setup"));
  const uri = `otpauth://totp/Scrapix:${encodeURIComponent(email)}?secret=${secret.toUpperCase()}&issuer=Scrapix`;
  return { secret, rawSecret: rawSecret ?? "", provisioningUri: uri };
}

export async function otpSetupConfirm(
  password: string,
  setup: OtpSetupStart,
  otp: string
): Promise<void> {
  const body = await rodauthPost("/auth/otp-setup", {
    password,
    otp_secret: setup.secret,
    otp_raw_secret: setup.rawSecret,
    otp,
  });
  if (!ok(body)) throw new Error(rodauthError(body, "Invalid authentication code"));
}

export async function otpDisable(password: string): Promise<void> {
  const body = await rodauthPost("/auth/otp-disable", { password });
  if (!ok(body)) throw new Error(rodauthError(body, "Could not disable TOTP"));
}

export async function recoveryCodes(password: string): Promise<string[]> {
  let body = await rodauthPost("/auth/recovery-codes", { password });
  if (!ok(body)) throw new Error(rodauthError(body, "Could not fetch recovery codes"));
  let codes = (body.codes as string[]) ?? [];
  if (codes.length === 0) {
    // Older enrollments predate auto-generation — create a batch now.
    await rodauthPost("/auth/recovery-codes", { password, add: "1" });
    body = await rodauthPost("/auth/recovery-codes", { password });
    codes = ok(body) ? ((body.codes as string[]) ?? []) : [];
  }
  return codes;
}

// ── WebAuthn passkeys ────────────────────────────────────────────────────────

function b64urlToBuffer(value: string): ArrayBuffer {
  const pad = "=".repeat((4 - (value.length % 4)) % 4);
  const raw = atob(value.replace(/-/g, "+").replace(/_/g, "/") + pad);
  const buf = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) buf[i] = raw.charCodeAt(i);
  return buf.buffer;
}

function bufferToB64url(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let raw = "";
  for (const b of bytes) raw += String.fromCharCode(b);
  return btoa(raw).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/* eslint-disable @typescript-eslint/no-explicit-any */
function credentialToJson(cred: PublicKeyCredential): Record<string, unknown> {
  const response = cred.response as any;
  const out: Record<string, unknown> = {
    id: cred.id,
    rawId: bufferToB64url(cred.rawId),
    type: cred.type,
    response: {
      clientDataJSON: bufferToB64url(response.clientDataJSON),
    },
  };
  const r = out.response as Record<string, unknown>;
  if (response.attestationObject) {
    r.attestationObject = bufferToB64url(response.attestationObject);
  }
  if (response.authenticatorData) {
    r.authenticatorData = bufferToB64url(response.authenticatorData);
    r.signature = bufferToB64url(response.signature);
    if (response.userHandle) r.userHandle = bufferToB64url(response.userHandle);
  }
  return out;
}

export async function webauthnRegister(password: string): Promise<void> {
  const start = await rodauthPost("/auth/webauthn-setup", { password });
  const options = start.webauthn_setup as any;
  if (!options) throw new Error(rodauthError(start, "Could not start passkey setup"));

  const publicKey: any = {
    ...options,
    challenge: b64urlToBuffer(options.challenge),
    user: { ...options.user, id: b64urlToBuffer(options.user.id) },
    excludeCredentials: (options.excludeCredentials ?? []).map((c: any) => ({
      ...c,
      id: b64urlToBuffer(c.id),
    })),
  };
  const cred = (await navigator.credentials.create({ publicKey })) as PublicKeyCredential;
  if (!cred) throw new Error("Passkey creation was cancelled");

  const finish = await rodauthPost("/auth/webauthn-setup", {
    password,
    webauthn_setup: JSON.stringify(credentialToJson(cred)),
    webauthn_setup_challenge: start.webauthn_setup_challenge,
    webauthn_setup_challenge_hmac: start.webauthn_setup_challenge_hmac,
  });
  if (!ok(finish)) throw new Error(rodauthError(finish, "Passkey registration failed"));
}

export async function webauthnAuth(): Promise<AuthUser> {
  const start = await rodauthPost("/auth/webauthn-auth", {});
  const options = start.webauthn_auth as any;
  if (!options) throw new Error(rodauthError(start, "Could not start passkey authentication"));

  const publicKey: any = {
    ...options,
    challenge: b64urlToBuffer(options.challenge),
    allowCredentials: (options.allowCredentials ?? []).map((c: any) => ({
      ...c,
      id: b64urlToBuffer(c.id),
    })),
  };
  const cred = (await navigator.credentials.get({ publicKey })) as PublicKeyCredential;
  if (!cred) throw new Error("Passkey authentication was cancelled");

  const finish = await rodauthPost("/auth/webauthn-auth", {
    webauthn_auth: JSON.stringify(credentialToJson(cred)),
    webauthn_auth_challenge: start.webauthn_auth_challenge,
    webauthn_auth_challenge_hmac: start.webauthn_auth_challenge_hmac,
  });
  if (!ok(finish)) throw new Error(rodauthError(finish, "Passkey authentication failed"));
  return getMe();
}
/* eslint-enable @typescript-eslint/no-explicit-any */
