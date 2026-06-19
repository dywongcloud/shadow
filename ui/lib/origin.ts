import "server-only";
import type { NextRequest } from "next/server";

/**
 * The app's PUBLIC origin for this request — i.e. the base for OAuth callback
 * URLs. Honors proxy/tunnel headers (ngrok, load balancers) so the callback
 * points at the address the user actually came in on (e.g.
 * `https://shadow.ngrok.pizza`) rather than the internal `http://localhost`.
 *
 * ngrok forwards the public host in the `Host` header and the real scheme in
 * `x-forwarded-proto`, so we reconstruct the origin from those.
 */
export function publicOrigin(req: NextRequest): string {
  const first = (v: string | null) => (v ? v.split(",")[0].trim() : "");
  const host =
    first(req.headers.get("x-forwarded-host")) ||
    first(req.headers.get("host")) ||
    req.nextUrl.host;
  const proto =
    first(req.headers.get("x-forwarded-proto")) ||
    req.nextUrl.protocol.replace(":", "") ||
    "https";
  return `${proto}://${host}`;
}
