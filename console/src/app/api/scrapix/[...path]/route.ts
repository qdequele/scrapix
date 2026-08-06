import { NextRequest } from "next/server";

const RUST_BACKEND = process.env.SCRAPIX_API_URL || "http://localhost:8080";
const SAAS_BACKEND = process.env.SAAS_API_URL || "";

/**
 * Path prefixes served by the Rails SaaS app (SCR-85 backend split).
 *
 * A prefix only routes to Rails once it appears in the SAAS_PREFIXES env var
 * (comma-separated, multi-segment allowed, e.g. "auth,account"), so cutover
 * is per-route-group and instantly reversible. SAAS_EXCLUDE_PREFIXES lists
 * deeper prefixes that stay on the Rust engine despite matching (e.g. the
 * Stripe routes under account/billing until phase 7). Longest match wins.
 * With SAAS_API_URL unset, all traffic goes to Rust regardless of prefixes.
 */
function parsePrefixes(value: string | undefined): string[] {
  return (value || "")
    .split(",")
    .map((p) => p.trim().replace(/^\//, "").replace(/\/$/, ""))
    .filter(Boolean);
}

const SAAS_PREFIXES = parsePrefixes(process.env.SAAS_PREFIXES);
const SAAS_EXCLUDE_PREFIXES = parsePrefixes(process.env.SAAS_EXCLUDE_PREFIXES);

function matchesPrefix(joined: string, prefix: string): boolean {
  return joined === prefix || joined.startsWith(`${prefix}/`);
}

function backendFor(path: string[]): string {
  if (!SAAS_BACKEND || path.length === 0) return RUST_BACKEND;
  const joined = path.join("/");
  if (SAAS_EXCLUDE_PREFIXES.some((p) => matchesPrefix(joined, p))) {
    return RUST_BACKEND;
  }
  if (SAAS_PREFIXES.some((p) => matchesPrefix(joined, p))) {
    return SAAS_BACKEND;
  }
  return RUST_BACKEND;
}

async function proxy(req: NextRequest, { params }: { params: Promise<{ path: string[] }> }) {
  const { path } = await params;
  const target = `${backendFor(path)}/${path.join("/")}${req.nextUrl.search}`;

  const headers: Record<string, string> = {
    "content-type": req.headers.get("content-type") || "application/json",
  };

  // Forward cookie header for session auth
  const cookie = req.headers.get("cookie");
  if (cookie) {
    headers["cookie"] = cookie;
  }

  // Forward API key header if present
  const apiKey = req.headers.get("x-api-key");
  if (apiKey) {
    headers["x-api-key"] = apiKey;
  }

  // Forward OAuth Bearer tokens (developer API / MCP clients)
  const authorization = req.headers.get("authorization");
  if (authorization) {
    headers["authorization"] = authorization;
  }

  const res = await fetch(target, {
    method: req.method,
    headers,
    body: req.method !== "GET" && req.method !== "HEAD" ? await req.text() : undefined,
  });

  // Build response headers, forwarding set-cookie from backend
  const responseHeaders = new Headers({
    "content-type": res.headers.get("content-type") || "application/json",
  });

  // Forward all set-cookie headers
  const setCookies = res.headers.getSetCookie();
  for (const sc of setCookies) {
    responseHeaders.append("set-cookie", sc);
  }

  return new Response(res.body, {
    status: res.status,
    headers: responseHeaders,
  });
}

export const GET = proxy;
export const POST = proxy;
export const PUT = proxy;
export const DELETE = proxy;
export const PATCH = proxy;
