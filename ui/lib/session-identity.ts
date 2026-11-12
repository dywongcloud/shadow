import "server-only";
import { headers } from "next/headers";
import { cookies } from "next/headers";
import { auth } from "@clerk/nextjs/server";

/** ONE verified identity for every server route that binds state to a signed-in user.
 *
 *  Clerk's server-side `auth()` can report signed-out on iOS Safari / a standalone PWA while the
 *  browser still holds a valid `__session` JWT. `/api/token` already works around that by verifying
 *  the browser's own session token before it mints. Every OTHER route (the GitHub credential
 *  binding, the Composio entity, the OAuth state) used bare `auth()`, so a valid connection read as
 *  `foreign` / entity `default` on exactly those devices. This resolves the SAME identity from a
 *  Clerk-signed token; nothing client-supplied is trusted, and failure is fail-closed.
 */

const APEX = (process.env.NEXT_PUBLIC_HIVE_PLATFORM_DOMAIN || "shadw.cloud").toLowerCase();

/** Verify a Clerk session token and return its subject. `authorizedParties` binds `azp` to our own
 *  origins; an unverifiable signature, expired token or foreign party yields null, never a passthrough. */
export async function verifyClerkSessionToken(
  token: string,
  authorizedParties?: string[],
): Promise<string | null> {
  const clerkSecret = process.env.CLERK_SECRET_KEY;
  if (!token || !clerkSecret) return null;
  try {
    const { verifyToken } = await import("@clerk/backend");
    const payload = await verifyToken(token, {
      secretKey: clerkSecret,
      ...(authorizedParties?.length ? { authorizedParties } : {}),
    });
    return payload?.sub ?? null;
  } catch {
    return null;
  }
}

/** Clerk only attaches `azp` to some session tokens. Mirror `/api/token`: apply the party check
 *  exactly when the claim is present; an absent claim still gets the full signature/expiry check. */
function hasAzp(token: string): boolean {
  try {
    const p = token.split(".")[1];
    if (!p) return false;
    const j = JSON.parse(Buffer.from(p, "base64url").toString("utf8")) as { azp?: unknown };
    return typeof j.azp === "string";
  } catch {
    return false;
  }
}

/** Accept only origins the platform itself controls. A forged `x-forwarded-host` cannot widen this:
 *  the `azp` inside a Clerk-signed token is minted by Clerk, not by the caller. */
export function clerkAuthorizedParties(token: string): string[] | undefined {
  if (!hasAzp(token)) return undefined;
  const configured = (process.env.HIVE_AUTHORIZED_PARTIES || "")
    .split(",")
    .map((v) => v.trim())
    .filter(Boolean);
  if (configured.length) return configured;
  return [
    `https://${APEX}`,
    `https://www.${APEX}`,
    "http://localhost:3001",
    "http://127.0.0.1:3001",
  ];
}

/** The browser's Clerk session cookie (`__session`, plus Clerk's suffixed variants). */
export async function browserSessionToken(): Promise<string | null> {
  try {
    const jar = await cookies();
    return jar.getAll().find((c) => c.name === "__session" || c.name.startsWith("__session_"))?.value ?? null;
  } catch {
    return null;
  }
}

/** The current verified user id, or null. Prefers Clerk's request session; when `auth()` cannot
 *  resolve it, verifies the `__session` JWT itself. Fails closed: a signed-out request never
 *  inherits another user's state. */
export async function verifiedUserId(): Promise<string | null> {
  const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY || !!process.env.CLERK_SECRET_KEY;
  if (clerkEnabled) {
    try {
      const { userId } = await auth();
      if (userId) return userId;
    } catch {}
  }
  const token = await browserSessionToken();
  if (!token) return null;
  return verifyClerkSessionToken(token, clerkAuthorizedParties(token));
}

/** The `Secure` flag for a cookie set in this request: https in production, false on an http LAN
 *  preview where a `Secure` cookie is silently dropped and the connect flow cannot complete. */
export async function cookieSecure(): Promise<boolean> {
  try {
    const h = await headers();
    const proto = (h.get("x-forwarded-proto") || "").split(",")[0].trim();
    if (proto) return proto === "https";
  } catch {}
  return process.env.NODE_ENV === "production";
}
