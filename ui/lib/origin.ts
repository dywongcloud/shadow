import "server-only";
import type { NextRequest } from "next/server";

/** The app's PUBLIC origin for this request, the base for OAuth callback URLs: rebuilt from `x-forwarded-host`/`host` and `x-forwarded-proto` so it is the address the user
 *  actually came in on (behind a proxy, tunnel or load balancer), not the internal `http://localhost`. */
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
