// Manually verified live under the platform's exact build/serve commands
// (`bunx --bun next build` then `bun run --bun start`, matching what
// hive-cloud/git.rs actually generates): middleware ran correctly and headers
// were set. `x-middleware-bun` always reports "not-bun" — Next.js runs
// middleware inside its OWN sandboxed Edge Runtime (a restricted, standardized
// global environment), which does not expose a `Bun` global even when the
// host process is genuinely Bun. This is Next.js's own design (identical
// under Node too — the Edge Runtime never exposes Node globals either), not a
// platform limitation: middleware behavior is IDENTICAL under Node and Bun by
// construction, since Next.js abstracts the runtime away for this specific
// execution context.
import { NextResponse } from "next/server";

export function middleware(request) {
  const res = NextResponse.next();
  res.headers.set("x-middleware-ran", "true");
  res.headers.set("x-middleware-bun", typeof Bun !== "undefined" ? (Bun.version ?? "unknown") : "not-bun");
  return res;
}

export const config = {
  matcher: "/:path*",
};
