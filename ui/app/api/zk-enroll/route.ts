import { NextResponse, type NextRequest } from "next/server";
import { auth, clerkClient } from "@clerk/nextjs/server";

// Background ZK roster enrollment — SECURED. Replaces the old client call that
// hit the node's open `/v1/zkauth/register` directly (which let any visitor
// self-enroll into any team). Here we verify, server-side via Clerk, that the
// signed-in user is actually a member of the requested team (Clerk org) before
// enrolling them on the node (locked behind HIVE_INTERNAL_TOKEN). No-op for
// teams the user doesn't belong to.
export const runtime = "nodejs";
export const dynamic = "force-dynamic";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const TOKEN = process.env.HIVE_INTERNAL_TOKEN || "";
const PERSONAL = new Set(["personal", "__personal__", ""]);

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

export async function POST(req: NextRequest) {
  const { userId } = await auth();
  if (!userId) return NextResponse.json({ ok: false, error: "not signed in" }, { status: 401 });
  const body = await req.json().catch(() => ({}) as any);
  const team = (body?.team as string) || "personal";

  // Only enroll for teams the user genuinely belongs to.
  if (!PERSONAL.has(team) && !(await isOrgMember(userId, team))) {
    return NextResponse.json({ ok: false, error: "not a member" }, { status: 403 });
  }
  const headers: Record<string, string> = { "content-type": "application/json", "x-hive-team": team };
  if (TOKEN) headers["x-hive-internal"] = TOKEN;
  try {
    await fetch(`${ADMIN}/v1/zkauth/register`, { method: "POST", headers, body: JSON.stringify({ user_id: userId }) });
  } catch {
    /* best-effort */
  }
  return NextResponse.json({ ok: true, team });
}
