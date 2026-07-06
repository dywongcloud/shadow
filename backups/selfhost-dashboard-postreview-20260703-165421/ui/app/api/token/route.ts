import { NextRequest, NextResponse } from "next/server";
import { auth, currentUser } from "@clerk/nextjs/server";

export const dynamic = "force-dynamic";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const INTERNAL = process.env.HIVE_INTERNAL_TOKEN || "";
const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY || !!process.env.CLERK_SECRET_KEY;

/**
 * Mint a short-lived platform JWT for the signed-in user and set it as an
 * httpOnly `hive_jwt` cookie, so the dashboard's same-origin `/cloud` calls are
 * authenticated at the admin ingress WITHOUT exposing the token to browser JS.
 *
 * The tenant claim is derived SERVER-SIDE from the verified Clerk session (active
 * org slug, else per-user `u_<id>`, else `personal` iff the account is the
 * platform owner) — never from client input — so it exactly mirrors the client's
 * `currentTeam()` namespace and cannot be spoofed to another tenant. The mint
 * itself is authorized to the backend with `x-hive-internal` (the mint is
 * server-only when the platform enforces JWT).
 *
 * SAFE / additive: when Clerk is disabled, returns `{ ok:false }` and sets no
 * cookie — the backend is unenforced in that mode, so the dashboard keeps working
 * exactly as before.
 */
export async function POST(_req: NextRequest) {
  if (!clerkEnabled) {
    return NextResponse.json({ ok: false, reason: "clerk-disabled" });
  }
  let userId: string | null = null;
  let orgSlug: string | null = null;
  let orgRole: string | null = null;
  try {
    const a = auth();
    userId = a.userId ?? null;
    orgSlug = a.orgSlug ?? null;
    orgRole = (a.orgRole as string | undefined) ?? null;
  } catch {
    /* auth() unavailable */
  }
  if (!userId) {
    return NextResponse.json({ ok: false, reason: "no-session" });
  }

  // Resolve the account email + owner status from the backend (authoritative
  // owner check via owner_email), mirroring the identity/sync path.
  let email = "";
  let name = "";
  try {
    const u = await currentUser();
    email = u?.primaryEmailAddress?.emailAddress || u?.emailAddresses?.[0]?.emailAddress || "";
    name = [u?.firstName, u?.lastName].filter(Boolean).join(" ") || u?.username || "";
  } catch {
    /* currentUser unavailable */
  }

  let isOwner = false;
  try {
    const r = await fetch(`${ADMIN}/v1/identity/sync`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ user: { id: userId, email, name }, org: orgSlug ? { id: orgSlug, slug: orgSlug, name: orgSlug } : null }),
      cache: "no-store",
    });
    if (r.ok) isOwner = !!(await r.json())?.is_owner;
  } catch {
    /* backend unreachable — fall through to a non-owner personal namespace */
  }

  // Canonical tenant namespace — IDENTICAL rule to the client's currentTeam():
  //   org scope → org slug; personal → owner keeps "personal", else per-user u_<id>.
  const tenant = orgSlug ? orgSlug : isOwner ? "personal" : `u_${userId}`;
  // Role: org role (admin→admin) when in an org; owner over one's own personal space.
  const role = orgSlug ? (orgRole?.includes("admin") ? "admin" : "member") : "owner";

  // Mint via the backend (server-only when enforced: proven by x-hive-internal).
  let token = "";
  let expiresIn = 3600;
  try {
    const r = await fetch(`${ADMIN}/v1/token`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(INTERNAL ? { "x-hive-internal": INTERNAL } : {}) },
      body: JSON.stringify({ sub: userId, tenant, role }),
      cache: "no-store",
    });
    if (!r.ok) {
      return NextResponse.json({ ok: false, reason: `mint-failed-${r.status}` }, { status: 502 });
    }
    const d = await r.json();
    token = d.token || "";
    expiresIn = d.expires_in || 3600;
  } catch {
    return NextResponse.json({ ok: false, reason: "mint-unreachable" }, { status: 502 });
  }
  if (!token) {
    // Dev backend (no secret) mints nothing meaningful / not enforced → no cookie.
    return NextResponse.json({ ok: true, tenant, enforced: false });
  }

  const res = NextResponse.json({ ok: true, tenant, role });
  res.cookies.set("hive_jwt", token, {
    httpOnly: true,
    secure: process.env.NODE_ENV === "production",
    sameSite: "lax",
    path: "/",
    maxAge: Math.max(60, expiresIn - 30),
  });
  return res;
}
