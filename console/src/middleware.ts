import { NextResponse, type NextRequest } from "next/server";

// In standalone mode request.nextUrl resolves to the server's own bind
// address (localhost:3002 behind Caddy), so absolute redirects built from it
// leak the internal host. Rebuild the target from the real request host.
function redirectTo(pathname: string, request: NextRequest) {
  const url = request.nextUrl.clone();
  url.pathname = pathname;
  const host =
    request.headers.get("x-forwarded-host") ?? request.headers.get("host");
  if (host) {
    url.host = host;
    url.port = "";
    const proto = request.headers.get("x-forwarded-proto");
    if (proto) url.protocol = proto;
  }
  return NextResponse.redirect(url);
}

export function middleware(request: NextRequest) {
  const hasSession = request.cookies.has("scrapix_session");
  const { pathname } = request.nextUrl;

  const isDashboardRoute = pathname.startsWith("/dashboard");
  const isAuthRoute =
    pathname.startsWith("/login") || pathname.startsWith("/signup");

  // Redirect unauthenticated users away from dashboard
  if (!hasSession && isDashboardRoute) {
    return redirectTo("/login", request);
  }

  // Redirect authenticated users away from auth pages to dashboard
  if (hasSession && isAuthRoute) {
    return redirectTo("/dashboard", request);
  }

  return NextResponse.next();
}

export const config = {
  // `api/` is excluded: API proxy routes need no redirects, and a request
  // that runs through middleware has its body capped at 10 MB, which would
  // truncate document uploads to POST /parse (up to 50 MB).
  matcher: [
    "/((?!api/|_next/static|_next/image|favicon.ico|.*\\.(?:svg|png|jpg|jpeg|gif|webp)$).*)",
  ],
};
