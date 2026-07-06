import { NextRequest, NextResponse } from "next/server";
import { auth, currentUser } from "@clerk/nextjs/server";

export const dynamic = "force-dynamic";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const INTERNAL = process.env.HIVE_INTERNAL_TOKEN || "";
const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY || !!process.env.CLERK_SECRET_KEY;
// Platform owner(s) — the accounts that keep the legacy "personal" tenant (all
// other accounts are isolated under `u_<uid>`). Mirrors the backend's
// HIVE_OWNER_EMAIL authoritative check. Determined HERE from the verified Clerk
// email rather than from `/v1/identity/sync`, because that endpoint is an
// auth-gated mutation and 401s at the enforced api.shadw.cloud ingress BEFORE we
// hold a token (the mint is the token bootstrap). The tenant we compute is only
// trusted by the backend because the mint (`/v1/token`) is `x-hive-internal`-
// gated — a non-owner can neither reach the mint nor be assigned "personal" here.
const OWNER_EMAILS = new Set(
  (process.env.HIVE_OWNER_EMAIL || "")
    .toLowerCase()
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean),
);

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

  // Authoritative owner check from the verified session email (see OWNER_EMAILS).
  const isOwner = !!email && OWNER_EMAILS.has(email.toLowerCase());

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

  // Best-effort: record identity on the backend NOW that we hold a bearer (the
  // enforced ingress rejects this mutation pre-auth). Never blocks the mint.
  try {
    await fetch(`${ADMIN}/v1/identity/sync`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${token}`, "x-hive-team": tenant },
      body: JSON.stringify({ user: { id: userId, email, name }, org: orgSlug ? { id: orgSlug, slug: orgSlug, name: orgSlug } : null }),
      cache: "no-store",
    });
  } catch {
    /* identity record is non-critical for auth */
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
