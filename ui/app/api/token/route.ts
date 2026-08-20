import { NextRequest, NextResponse } from "next/server";
import { auth, currentUser, clerkClient } from "@clerk/nextjs/server";

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

/** Read the `platform_admin` claim out of the JWT the backend just minted for
 * this session. The claim is derived by the BACKEND from its own
 * owner/admin-email config at mint time, so decoding it here (no verification —
 * this server trusts its own backend's response) mirrors the operator status
 * the enforced endpoints will actually apply. `null` = undecodable/absent, and
 * callers must treat that as UNKNOWN, never as "not an operator". */
function jwtPlatformAdmin(token: string): boolean | null {
  try {
    const part = token.split(".")[1];
    if (!part) return null;
    const claims = JSON.parse(Buffer.from(part, "base64url").toString("utf8")) as { platform_admin?: unknown };
    return claims.platform_admin === true;
  } catch {
    return null;
  }
}

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
export async function POST(req: NextRequest) {
  if (!clerkEnabled) {
    // Dev-mint branch (bn-local-dev-clerk-hydration-gap): with the auth bypass
    // on (the same NODE_ENV-gated condition as ui/proxy.ts's hatch) and no
    // Clerk keys, mint a LOCAL JWT for the requested team so full-page local
    // testing exercises the REAL production mint→cookie→JWT-tenant path
    // (401-remint included) instead of reading the anonymous/empty tenant.
    // Never production-reachable by construction.
    const devMint = process.env.HIVE_AUTH_BYPASS === "1" && process.env.NODE_ENV !== "production";
    if (!devMint) {
      return NextResponse.json({ ok: false, reason: "clerk-disabled" });
    }
    let team = "personal";
    try {
      const t = String(((await req.json()) as { team?: unknown })?.team ?? "").trim();
      if (t && t !== "__pending__") team = t;
    } catch {
      /* no/invalid body → default to personal */
    }
    try {
      const r = await fetch(`${ADMIN}/v1/token`, {
        method: "POST",
        headers: { "content-type": "application/json", ...(INTERNAL ? { "x-hive-internal": INTERNAL } : {}) },
        // `email` lets the backend derive `platform_admin` from its own
        // HIVE_OWNER_EMAIL — the dev mint asserts no platform claim itself.
        body: JSON.stringify({ sub: "local-dev", tenant: team, role: "owner", email: process.env.HIVE_OWNER_EMAIL || "" }),
        cache: "no-store",
      });
      if (!r.ok) {
        return NextResponse.json({ ok: false, reason: `mint-failed-${r.status}` }, { status: 502 });
      }
      const d = await r.json();
      const token = d.token || "";
      const expiresIn = d.expires_in || 3600;
      if (!token) {
        return NextResponse.json({ ok: true, tenant: team, enforced: false });
      }
      const res = NextResponse.json({ ok: true, tenant: team, role: "owner", dev: true, platform_admin: jwtPlatformAdmin(token) ?? true });
      res.cookies.set("hive_jwt", token, {
        httpOnly: true,
        secure: false, // local-only branch (NODE_ENV !== "production" above)
        sameSite: "lax",
        path: "/",
        maxAge: Math.max(60, expiresIn - 30),
      });
      return res;
    } catch {
      return NextResponse.json({ ok: false, reason: "mint-unreachable" }, { status: 502 });
    }
  }
  let userId: string | null = null;
  try {
    userId = (await auth()).userId ?? null;
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
  // The user's OWN personal namespace: the owner keeps the legacy "personal",
  // every other account is isolated under `u_<uid>`. A non-owner can NEVER be
  // assigned "personal" here.
  const personalTenant = isOwner ? "personal" : `u_${userId}`;

  // The client sends the team it is CURRENTLY viewing (its `currentTeam()`), which
  // is the source of truth for the dashboard view — NOT Clerk's server-side active
  // org, which lags/doesn't follow `setActive()` and left the JWT stuck on the
  // owner's "personal" tenant regardless of the selected team (the isolation bug).
  //
  // ZERO-TRUST: we never mint the requested tenant on trust. An ORG tenant is
  // granted ONLY after verifying the user is a MEMBER of that org (Clerk is the
  // authority); the personal namespace is granted only as the user's OWN. Anything
  // unrecognized/unauthorized falls back to the caller's own personal namespace —
  // never another tenant's, and never the owner's "personal" for a non-owner.
  let requested = "";
  try {
    requested = String(((await req.json()) as { team?: unknown })?.team ?? "").trim();
  } catch {
    /* no/invalid body → default to personal */
  }

  let tenant = personalTenant;
  let role: "owner" | "admin" | "member" = "owner";
  const wantsPersonal = !requested || requested === "personal" || requested === personalTenant || requested === "__pending__";
  if (!wantsPersonal) {
    // An org slug — authorize against the user's actual Clerk org memberships.
    try {
      const memberships = await (await clerkClient()).users.getOrganizationMembershipList({ userId, limit: 100 });
      const list = Array.isArray(memberships) ? memberships : memberships?.data ?? [];
      const m = list.find((mm: { organization: { slug?: string | null; id: string } }) => {
        const o = mm.organization;
        return (o.slug || o.id) === requested;
      });
      if (m) {
        tenant = requested;
        role = String((m as { role?: string }).role || "").includes("admin") ? "admin" : "member";
      }
      // else: not a member → stays personalTenant (safe; never grants the org).
    } catch {
      /* membership lookup failed → safe default (personalTenant) */
    }
  }
  // For the personal namespace, the caller owns it (owner over "personal", or the
  // user over their own `u_<uid>`).
  const orgSlug = tenant !== personalTenant ? tenant : null;

  // Mint via the backend (server-only when enforced: proven by x-hive-internal).
  let token = "";
  let expiresIn = 3600;
  try {
    const r = await fetch(`${ADMIN}/v1/token`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(INTERNAL ? { "x-hive-internal": INTERNAL } : {}) },
      // `email` lets the backend independently derive `platform_admin` from
      // its OWN owner_email config (defense in depth — the backend never
      // trusts a client-asserted platform-admin claim). `role` here is only
      // ever the TENANT-scoped role; it is never used for platform authority.
      body: JSON.stringify({ sub: userId, tenant, role, email }),
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
    // Unenforced means every operator gate passes, so report operator status UP.
    return NextResponse.json({ ok: true, tenant, enforced: false, platform_admin: true });
  }

  // Best-effort: record identity on the backend NOW that we hold a bearer (the
  // enforced ingress rejects this mutation pre-auth). Never blocks the mint.
  try {
    await fetch(`${ADMIN}/v1/identity/sync`, {
      method: "POST",
      // x-hive-internal proves this sync is the SERVER-SIDE mint (email is the
      // Clerk-verified one) — the backend only mirrors org membership into the
      // team roster for internally-proven syncs, so a browser-forged body
      // can't write an arbitrary email into a roster.
      headers: { "content-type": "application/json", authorization: `Bearer ${token}`, "x-hive-team": tenant, "x-hive-internal": INTERNAL },
      body: JSON.stringify({ user: { id: userId, email, name }, org: orgSlug ? { id: orgSlug, slug: orgSlug, name: orgSlug } : null }),
      cache: "no-store",
    });
  } catch {
    /* identity record is non-critical for auth */
  }

  // Surface the backend-derived operator status (decoded from the JWT we just
  // minted) so operator-only surfaces can disclose the boundary instead of
  // failing with a bare 403. Omitted when the claim is undecodable — the client
  // treats an absent field as UNKNOWN and leaves those surfaces untouched.
  const platformAdmin = jwtPlatformAdmin(token);
  const res = NextResponse.json({ ok: true, tenant, role, ...(platformAdmin !== null ? { platform_admin: platformAdmin } : {}) });
  res.cookies.set("hive_jwt", token, {
    httpOnly: true,
    secure: process.env.NODE_ENV === "production",
    sameSite: "lax",
    path: "/",
    maxAge: Math.max(60, expiresIn - 30),
  });
  return res;
}
