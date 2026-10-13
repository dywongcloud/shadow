import { NextRequest, NextResponse } from "next/server";
import { auth, currentUser, clerkClient } from "@clerk/nextjs/server";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const INTERNAL = process.env.HIVE_INTERNAL_TOKEN || "";
const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY || !!process.env.CLERK_SECRET_KEY;
const OWNER_EMAILS = new Set(
  (process.env.HIVE_OWNER_EMAIL || "")
    .toLowerCase()
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean),
);

/** Reads the `platform_admin` claim from the JWT the backend just minted (unverified: this server trusts its own backend);
 *  `null` = undecodable or absent = UNKNOWN, never "not an operator". */
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

/** Parent-domain scope for the `hive_jwt` cookie (e.g. `.shadw.cloud`), so the same cookie also rides the sandbox shell's
 *  `wss://api.<domain>` handshake (a host-only cookie reaches no other subdomain, and a WebSocket cannot add a header).
 *  `undefined` for localhost and single-label hosts, where browsers reject a `Domain` attribute. */
function cookieDomain(req: NextRequest): string | undefined {
  const host = (req.headers.get("x-forwarded-host") || req.headers.get("host") || "")
    .split(",")[0]
    .trim()
    .split(":")[0]
    .toLowerCase();
  if (!host || host === "localhost" || host === "127.0.0.1") return undefined;
  const parts = host.split(".");
  if (parts.length < 2) return undefined;
  return `.${parts.slice(-2).join(".")}`;
}

/** Mints a short-lived platform JWT for the signed-in user and sets it as the httpOnly `hive_jwt` cookie. The tenant claim is
 *  derived SERVER-SIDE from the verified Clerk session, never from client input, and the mint call to the backend carries
 *  `x-hive-internal`. Clerk disabled: `{ ok:false }` and no cookie (the backend is unenforced). */
export async function POST(req: NextRequest) {
  if (!clerkEnabled) {
    const devMint = process.env.HIVE_AUTH_BYPASS === "1" && process.env.NODE_ENV !== "production";
    if (!devMint) {
      return NextResponse.json({ ok: false, reason: "clerk-disabled" });
    }
    let team = "personal";
    try {
      const t = String(((await req.json()) as { team?: unknown })?.team ?? "").trim();
      if (t && t !== "__pending__") team = t;
    } catch {}
    try {
      const firstOwnerEmail = (process.env.HIVE_OWNER_EMAIL || "").split(",")[0].trim();
      const r = await fetch(`${ADMIN}/v1/token`, {
        method: "POST",
        headers: { "content-type": "application/json", ...(INTERNAL ? { "x-hive-internal": INTERNAL } : {}) },
        body: JSON.stringify({ sub: "local-dev", tenant: team, role: "owner", email: firstOwnerEmail }),
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
        secure: false,
        sameSite: "lax",
        domain: cookieDomain(req),
        path: "/",
        maxAge: Math.max(60, expiresIn - 30),
      });
      return res;
    } catch {
      return NextResponse.json({ ok: false, reason: "mint-unreachable" }, { status: 502 });
    }
  }
/** Verifies a Clerk session token the BROWSER holds (JWKS, plus `azp` against HIVE_AUTHORIZED_PARTIES when present) and returns
 *  its user id, else null: the fallback for a Clerk DEV instance whose server-side `auth()` reports signed-out without the
 *  `__clerk_db_jwt` cookie. Trusts nothing the client asserts and grants no more than the cookie path would. */
async function userIdFromSessionToken(tok: string): Promise<string | null> {
  try {
    const { verifyToken } = await import("@clerk/backend");
    let azp: unknown;
    try {
      const p = tok.split(".")[1];
      if (p) azp = (JSON.parse(Buffer.from(p, "base64url").toString("utf8")) as { azp?: unknown }).azp;
    } catch {
      azp = undefined;
    }
    const payload = await verifyToken(tok, {
      secretKey: process.env.CLERK_SECRET_KEY,
      ...(typeof azp === "string"
        ? {
            authorizedParties: (process.env.HIVE_AUTHORIZED_PARTIES || "http://localhost:3001,http://127.0.0.1:3001")
              .split(",")
              .map((v) => v.trim())
              .filter(Boolean),
          }
        : {}),
    });
    return payload?.sub ?? null;
  } catch {
    return null;
  }
}

  let clerkTokenFromBody = "";
  let requested = "";
  try {
    const body = (await req.json()) as { team?: unknown; clerk_token?: unknown } | null;
    requested = String(body?.team ?? "").trim();
    if (typeof body?.clerk_token === "string") clerkTokenFromBody = body.clerk_token.trim();
  } catch {}

  let userId: string | null = null;
  let usedSessionToken = false;
  try {
    userId = (await auth()).userId ?? null;
  } catch {}
  if (!userId) {
    const bearer = req.headers.get("authorization");
    const headerToken =
      bearer && /^Bearer\s+/i.test(bearer) ? bearer.replace(/^Bearer\s+/i, "").trim() : "";
    const tok = headerToken || (typeof clerkTokenFromBody === "string" ? clerkTokenFromBody : "");
    if (tok) {
      const verified = await userIdFromSessionToken(tok);
      if (verified) {
        userId = verified;
        usedSessionToken = true;
      }
    }
  }
  if (!userId) {
    return NextResponse.json({ ok: false, reason: "no-session" });
  }

  let email = "";
  let name = "";
  if (usedSessionToken) {
    try {
      const u = await (await clerkClient()).users.getUser(userId);
      email = u?.primaryEmailAddress?.emailAddress || u?.emailAddresses?.[0]?.emailAddress || "";
      name = [u?.firstName, u?.lastName].filter(Boolean).join(" ") || u?.username || "";
    } catch {}
  } else {
    try {
      const u = await currentUser();
      email = u?.primaryEmailAddress?.emailAddress || u?.emailAddresses?.[0]?.emailAddress || "";
      name = [u?.firstName, u?.lastName].filter(Boolean).join(" ") || u?.username || "";
    } catch {}
  }

  const isOwner = !!email && OWNER_EMAILS.has(email.toLowerCase());
  const personalTenant = isOwner ? "personal" : `u_${userId}`;

  let tenant = personalTenant;
  let role: "owner" | "admin" | "member" = "owner";
  const wantsPersonal = !requested || requested === "personal" || requested === personalTenant || requested === "__pending__";
  if (!wantsPersonal) {
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
    } catch {}
  }
  const orgSlug = tenant !== personalTenant ? tenant : null;

  let token = "";
  let expiresIn = 3600;
  try {
    const r = await fetch(`${ADMIN}/v1/token`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(INTERNAL ? { "x-hive-internal": INTERNAL } : {}) },
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
    return NextResponse.json({ ok: true, tenant, enforced: false, platform_admin: true });
  }

  try {
    await fetch(`${ADMIN}/v1/identity/sync`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${token}`, "x-hive-team": tenant, "x-hive-internal": INTERNAL },
      body: JSON.stringify({ user: { id: userId, email, name }, org: orgSlug ? { id: orgSlug, slug: orgSlug, name: orgSlug } : null }),
      cache: "no-store",
    });
  } catch {}

  const platformAdmin = jwtPlatformAdmin(token);
  const res = NextResponse.json({ ok: true, tenant, role, ...(platformAdmin !== null ? { platform_admin: platformAdmin } : {}) });
  const domain = cookieDomain(req);
  if (domain) {
    res.cookies.set("hive_jwt", "", { httpOnly: true, secure: true, sameSite: "lax", path: "/", maxAge: 0 });
  }
  res.cookies.set("hive_jwt", token, {
    httpOnly: true,
    secure: process.env.NODE_ENV === "production",
    sameSite: "lax",
    domain,
    path: "/",
    maxAge: Math.max(60, expiresIn - 30),
  });
  return res;
}
