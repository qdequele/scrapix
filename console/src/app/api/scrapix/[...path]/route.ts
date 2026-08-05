import { NextRequest } from "next/server";

const RUST_BACKEND = process.env.SCRAPIX_API_URL || "http://localhost:8080";
const SAAS_BACKEND = process.env.SAAS_API_URL || "";

/**
 * Path prefixes served by the Rails SaaS app (SCR-85 backend split).
 *
 * A prefix only routes to Rails once it appears in SAAS_PREFIXES env var
 * (comma-separated), so cutover is per-route-group and instantly reversible:
 *   SAAS_API_URL=http://localhost:8081 SAAS_PREFIXES=analytics,configs
 * Everything else stays on the Rust engine. With SAAS_API_URL unset, all
 * traffic goes to Rust regardless of prefixes.
 */
const SAAS_PREFIXES = new Set(
  (process.env.SAAS_PREFIXES || "")
    .split(",")
    .map((p) => p.trim().replace(/^\//, ""))
    .filter(Boolean),
);

function backendFor(path: string[]): string {
  if (SAAS_BACKEND && path.length > 0 && SAAS_PREFIXES.has(path[0])) {
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
