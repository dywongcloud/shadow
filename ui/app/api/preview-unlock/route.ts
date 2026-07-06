import { NextResponse, type NextRequest } from "next/server";
import { auth, clerkClient } from "@clerk/nextjs/server";

// SECURITY + TRUST BOUNDARY. The /preview-unlock page (which shows the ZK-proof
// animation) calls this route. It verifies, server-side via Clerk, that the
// signed-in user is actually a MEMBER of the deployment's team (Clerk org) before
// enrolling them in the ZK roster and minting an access proof on the node. The
// node can't verify Clerk itself, so the node's zkauth endpoints are kept off the
// public proxy (next.config) — only this server reaches them. Returns JSON so the
// page can stage the proof visualization and then redirect to `url`.
export const runtime = "nodejs";
export const dynamic = "force-dynamic";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const TOKEN = process.env.HIVE_INTERNAL_TOKEN || "";
const PERSONAL = new Set(["personal", "__personal__", ""]);
const DEPLOY_DOMAIN = (process.env.NEXT_PUBLIC_DEPLOYMENT_DOMAIN || "").trim().replace(/^\.+|\.+$/g, "");

/**
 * Is `host` a platform-issued deployment domain? Blocks the open-redirect /
 * ZK-proof-exfiltration vector where an attacker sets `?host=attacker.example`
 * on a crafted link — the real proof/message minted for a legitimate member
 * would otherwise be shipped straight to that arbitrary external origin.
 *
 * Covers every STANDARD alias shape (`.localhost` dev, region-encoded
 * `*.<code>.ngrok.pizza`, and the configured `NEXT_PUBLIC_DEPLOYMENT_DOMAIN`).
 * KNOWN GAP: a project's own CUSTOM domain (the Domains feature) is not in
 * this list — this check intentionally only defends against a truly
 * arbitrary third-party host; a stronger fix would resolve the deployment's
 * exact registered host(s) for `project` server-side and require an exact
 * match, which needs new backend plumbing beyond this pass's scope.
 */
function isKnownDeploymentHost(host: string): boolean {
  const h = host.toLowerCase();
  if (h === "localhost" || h.endsWith(".localhost")) return true;
  if (h.endsWith(".ngrok.pizza")) return true;
  if (DEPLOY_DOMAIN && (h === DEPLOY_DOMAIN || h.endsWith(`.${DEPLOY_DOMAIN}`))) return true;
  return false;
}

/** Is `userId` a member of the Clerk org whose slug (or id) == `team`? */
async function isOrgMember(userId: string, team: string): Promise<boolean> {
  try {
    const client = (typeof clerkClient === "function" ? await (clerkClient as any)() : clerkClient) as any;
    const res = await client.users.getOrganizationMembershipList({ userId, limit: 200 });
    const list: any[] = res?.data ?? res ?? [];
    return list.some((m) => {
      const o = m.organization || {};
      return o.slug === team || o.id === team;
    });
  } catch {
    return false;
  }
}

export async function GET(req: NextRequest) {
  const sp = req.nextUrl.searchParams;
  const host = sp.get("host") || "";
  const project = sp.get("project") || "";
  const team = sp.get("team") || "personal";
  const next = sp.get("next") || "/";
  if (!host || !project) return NextResponse.json({ ok: false, error: "missing deployment info" }, { status: 400 });
  if (!isKnownDeploymentHost(host)) {
    return NextResponse.json({ ok: false, error: "unrecognized deployment host" }, { status: 400 });
  }

  const { userId } = await auth();
  if (!userId) return NextResponse.json({ ok: false, signin: true, error: "sign in required" }, { status: 401 });

  // Org-team previews require verified membership. Personal previews are allowed
  // for any signed-in user (the node's "personal" tenant is a shared sentinel).
  if (!PERSONAL.has(team) && !(await isOrgMember(userId, team))) {
    return NextResponse.json(
      { ok: false, error: `You don't have access to the "${team}" team's preview deployments.` },
      { status: 403 },
    );
  }

  // Verified member — enroll (idempotent) + mint a proof on the node.
  const headers: Record<string, string> = { "content-type": "application/json", "x-hive-team": team };
  if (TOKEN) headers["x-hive-internal"] = TOKEN;
  try {
    await fetch(`${ADMIN}/v1/zkauth/register`, { method: "POST", headers, body: JSON.stringify({ user_id: userId }) });
    const r = await fetch(`${ADMIN}/v1/zkauth/preview-proof`, {
      method: "POST",
      headers,
      body: JSON.stringify({ user_id: userId, project }),
    });
    if (!r.ok) {
      const detail = await r.text().catch(() => "");
      console.error(`[preview-unlock] member ${userId} of ${team}: proof mint failed ${r.status}: ${detail}`);
      return NextResponse.json(
        { ok: false, error: `Preview service error (${r.status}). The deployment may still be starting.` },
        { status: 502 },
      );
    }
    const d = await r.json();
    const scheme = host.includes("localhost") ? "http" : "https";
    const url = `${scheme}://${host}/_shadw/zk?p=${encodeURIComponent(d.proof)}&m=${encodeURIComponent(d.message)}&t=${encodeURIComponent(d.team)}&next=${encodeURIComponent(next)}`;
    // Expose proof metadata so the page can visualize it (the proof itself stays
    // server↔deployment; we only surface a short fingerprint + size).
    const proof: string = d.proof || "";
    return NextResponse.json({
      ok: true,
      url,
      team: d.team,
      proofBytes: Math.ceil(proof.length / 2),
      fingerprint: proof.slice(0, 16),
      nullifier: (d.message || "").toString().slice(0, 12),
    });
  } catch (e) {
    return NextResponse.json({ ok: false, error: "unlock failed" }, { status: 502 });
  }
}
