import { NextResponse, type NextRequest } from "next/server";
import { auth, clerkClient } from "@clerk/nextjs/server";

// Trust boundary: verifies via Clerk that the signed-in user is a MEMBER of the deployment's team before enrolling them in the ZK roster and
// minting a proof on the node (the node cannot verify Clerk, so its zkauth endpoints stay off the public proxy).
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const TOKEN = process.env.HIVE_INTERNAL_TOKEN || "";
const PERSONAL = new Set(["personal", "__personal__", ""]);
const DEPLOY_DOMAIN = (process.env.NEXT_PUBLIC_DEPLOYMENT_DOMAIN || "").trim().replace(/^\.+|\.+$/g, "");

/** Whether `host` is a platform-issued deployment domain (the `.localhost` dev shapes and the NEXT_PUBLIC_DEPLOYMENT_DOMAIN apps wildcard):
 *  blocks an open redirect that would ship the minted proof to an arbitrary origin. A project's own CUSTOM domain is NOT matched, so a proof
 *  for one is refused here. */
function isKnownDeploymentHost(host: string): boolean {
  const h = host.toLowerCase();
  if (h === "localhost" || h.endsWith(".localhost")) return true;
  if (DEPLOY_DOMAIN && (h === DEPLOY_DOMAIN || h.endsWith(`.${DEPLOY_DOMAIN}`))) return true;
  return false;
}

/** Cause-specific copy for an unlock failure (an access-service fault is not "the deployment may still be starting"). */
function unlockError(status: number, stage: "enroll" | "mint"): string {
  if (status === 403) return "This platform rejected the unlock request. The dashboard is not authorized to grant preview access.";
  if (status === 404) return "The preview access service is unavailable on this node. Please try again.";
  if (status >= 500) return "The preview access service failed while verifying your membership. Please try again.";
  return `Could not ${stage === "enroll" ? "enroll you for" : "grant"} preview access (${status}).`;
}

/** The configured platform owner(s): HIVE_OWNER_EMAIL is a COMMA-SEPARATED list, parsed into a set exactly like `/api/token` does. */
function ownerEmails(): Set<string> {
  return new Set(
    (process.env.HIVE_OWNER_EMAIL || "")
      .toLowerCase()
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean),
  );
}

/** A personal namespace belongs to exactly ONE user, so membership is checked directly rather than through Clerk orgs: `u_<clerkUid>` must match
 *  exactly, and the legacy shared `personal` belongs to the platform owner(s) by HIVE_OWNER_EMAIL (permissive when none is configured). */
async function personalAccessOk(team: string, userId: string): Promise<boolean> {
  if (team.startsWith("u_")) return team === `u_${userId}`;
  const owners = ownerEmails();
  if (owners.size === 0) return true;
  try {
    const client = (typeof clerkClient === "function" ? await (clerkClient as any)() : clerkClient) as any;
    const u = await client.users.getUser(userId);
    const emails: string[] = (u?.emailAddresses ?? []).map((e: any) => String(e?.emailAddress || "").toLowerCase());
    return emails.some((e) => owners.has(e));
  } catch {
    return true;
  }
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
  const paramTeam = sp.get("team") || "personal";
  const next = sp.get("next") || "/";
  if (!host || !project) return NextResponse.json({ ok: false, error: "missing deployment info" }, { status: 400 });
  if (!isKnownDeploymentHost(host)) {
    return NextResponse.json({ ok: false, error: "unrecognized deployment host" }, { status: 400 });
  }

  const { userId } = await auth();
  if (!userId) return NextResponse.json({ ok: false, signin: true, error: "sign in required" }, { status: 401 });

  let team = paramTeam;
  try {
    const th: Record<string, string> = {};
    if (TOKEN) th["x-hive-internal"] = TOKEN;
    const tr = await fetch(`${ADMIN}/v1/zkauth/project-team?project=${encodeURIComponent(project)}`, { headers: th });
    if (tr.ok) {
      const td = await tr.json();
      const resolved = String(td?.team || "").trim();
      if (resolved && resolved !== "__untagged__") team = resolved;
    } else if (tr.status === 403) {
      console.error(`[preview-unlock] project-team resolution refused (403) for ${project}; check HIVE_INTERNAL_TOKEN`);
      return NextResponse.json({ ok: false, error: unlockError(403, "enroll") }, { status: 502 });
    }
  } catch {}

  const personal = PERSONAL.has(team) || team.startsWith("u_");
  const allowed = personal ? await personalAccessOk(team, userId) : await isOrgMember(userId, team);
  if (!allowed) {
    return NextResponse.json(
      {
        ok: false,
        error: personal
          ? "This preview belongs to another user's personal workspace."
          : `You don't have access to the "${team}" team's preview deployments.`,
      },
      { status: 403 },
    );
  }

  const headers: Record<string, string> = { "content-type": "application/json", "x-hive-team": team };
  if (TOKEN) headers["x-hive-internal"] = TOKEN;
  try {
    const reg = await fetch(`${ADMIN}/v1/zkauth/register`, {
      method: "POST",
      headers,
      body: JSON.stringify({ user_id: userId }),
    });
    if (!reg.ok) {
      const detail = await reg.text().catch(() => "");
      console.error(`[preview-unlock] member ${userId} of ${team}: enrollment failed ${reg.status}: ${detail}`);
      return NextResponse.json({ ok: false, error: unlockError(reg.status, "enroll") }, { status: 502 });
    }
    const r = await fetch(`${ADMIN}/v1/zkauth/preview-proof`, {
      method: "POST",
      headers,
      body: JSON.stringify({ user_id: userId, project, host }),
    });
    if (!r.ok) {
      const detail = await r.text().catch(() => "");
      console.error(`[preview-unlock] member ${userId} of ${team}: proof mint failed ${r.status}: ${detail}`);
      return NextResponse.json({ ok: false, error: unlockError(r.status, "mint") }, { status: 502 });
    }
    const d = await r.json();
    const scheme = host.includes("localhost") ? "http" : "https";
    const url = `${scheme}://${host}/_shadw/zk?p=${encodeURIComponent(d.proof)}&m=${encodeURIComponent(d.message)}&t=${encodeURIComponent(d.team)}&next=${encodeURIComponent(next)}`;
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
