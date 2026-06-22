import { NextResponse, type NextRequest } from "next/server";
import { auth, clerkClient } from "@clerk/nextjs/server";

// SECURITY: the deployment gate bounces protected-preview navigations here. This
// route is the TRUST BOUNDARY — it verifies (server-side, via Clerk) that the
// signed-in user is actually a MEMBER of the deployment's team (Clerk org) before
// enrolling them in the ZK roster and minting an access proof. The node can't
// verify Clerk itself, so enrollment/minting on the node is locked behind
// HIVE_INTERNAL_TOKEN, which only this server attaches. A non-member is denied.
export const runtime = "nodejs";
export const dynamic = "force-dynamic";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const TOKEN = process.env.HIVE_INTERNAL_TOKEN || "";
const PERSONAL = new Set(["personal", "__personal__", ""]);

function deny(team: string) {
  return new NextResponse(
    `<!doctype html><meta charset=utf-8><title>Preview locked</title>` +
      `<div style="font:15px system-ui;max-width:32rem;margin:18vh auto;text-align:center;color:#111">` +
      `<h1 style="font-size:18px">Preview locked</h1>` +
      `<p style="color:#666">You don't have access to the <b>${team}</b> team's preview deployments.</p>` +
      `<p><a href="/" style="color:#0070f3">Back to dashboard</a></p></div>`,
    { status: 403, headers: { "content-type": "text/html; charset=utf-8" } },
  );
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
  if (!host || !project) return new NextResponse("missing deployment info", { status: 400 });

  const { userId } = await auth();
  if (!userId) {
    // Sign in, then return to this exact unlock URL to complete the flow.
    const back = encodeURIComponent(req.nextUrl.pathname + req.nextUrl.search);
    return NextResponse.redirect(new URL(`/sign-in?redirect_url=${back}`, req.url));
  }

  // Org-team previews require verified membership. Personal previews are allowed
  // for any signed-in user (the node's "personal" tenant is a shared sentinel).
  if (!PERSONAL.has(team) && !(await isOrgMember(userId, team))) {
    return deny(team);
  }

  // Verified member — enroll (idempotent) + mint a proof on the node,
  // server-to-server. (TOKEN is optional defense-in-depth; only sent if both the
  // node and dashboard set the same HIVE_INTERNAL_TOKEN.)
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
      // The user IS a verified member here — a failure now is a node/config issue
      // (e.g. node has HIVE_INTERNAL_TOKEN set but the dashboard doesn't), NOT an
      // access problem. Surface it distinctly instead of "Preview locked".
      const detail = await r.text().catch(() => "");
      console.error(`[preview-unlock] member ${userId} of ${team}: node proof mint failed ${r.status}: ${detail}`);
      return new NextResponse(
        `<!doctype html><meta charset=utf-8><title>Preview unavailable</title>` +
          `<div style="font:15px system-ui;max-width:34rem;margin:18vh auto;text-align:center;color:#111">` +
          `<h1 style="font-size:18px">Couldn't unlock this preview</h1>` +
          `<p style="color:#666">You're a member of <b>${team}</b>, but the preview service returned an error (${r.status}). ` +
          `If the node sets <code>HIVE_INTERNAL_TOKEN</code>, set the same value in the dashboard env.</p>` +
          `<p><a href="/" style="color:#0070f3">Back to dashboard</a></p></div>`,
        { status: 502, headers: { "content-type": "text/html; charset=utf-8" } },
      );
    }
    const d = await r.json();
    const scheme = host.includes("localhost") ? "http" : "https";
    const url = `${scheme}://${host}/_shadw/zk?p=${encodeURIComponent(d.proof)}&m=${encodeURIComponent(d.message)}&t=${encodeURIComponent(d.team)}&next=${encodeURIComponent(next)}`;
    return NextResponse.redirect(url);
  } catch {
    return new NextResponse("unlock failed", { status: 502 });
  }
}
