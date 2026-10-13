import { NextResponse, type NextRequest } from "next/server";
import { auth, clerkClient } from "@clerk/nextjs/server";

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

/** Background ZK roster enrollment, secured: Clerk verifies server-side that the signed-in user is a member of the requested team (Clerk org) before they are enrolled on the node
 *  (`/v1/zkauth/register`, locked behind HIVE_INTERNAL_TOKEN); a no-op for teams the user does not belong to. The node's register endpoint is never called from the browser. */
export async function POST(req: NextRequest) {
  const { userId } = await auth();
  if (!userId) return NextResponse.json({ ok: false, error: "not signed in" }, { status: 401 });
  const body = await req.json().catch(() => ({}) as any);
  const team = (body?.team as string) || "personal";

  if (!PERSONAL.has(team) && !(await isOrgMember(userId, team))) {
    return NextResponse.json({ ok: false, error: "not a member" }, { status: 403 });
  }
  const headers: Record<string, string> = { "content-type": "application/json", "x-hive-team": team };
  if (TOKEN) headers["x-hive-internal"] = TOKEN;
  try {
    await fetch(`${ADMIN}/v1/zkauth/register`, { method: "POST", headers, body: JSON.stringify({ user_id: userId }) });
  } catch {}
  return NextResponse.json({ ok: true, team });
}
