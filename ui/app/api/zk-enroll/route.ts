import { NextResponse, type NextRequest } from "next/server";
import { auth } from "@clerk/nextjs/server";
import { teamAccess } from "@/lib/preview-access";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const TOKEN = process.env.HIVE_INTERNAL_TOKEN || "";

/** Background ZK roster enrollment, secured: the same `teamAccess` check preview unlock runs (Clerk org membership, a user's own `u_<id>` namespace, the legacy `personal` roster only for the
 *  platform owner(s)) must pass before the user is enrolled on the node (`/v1/zkauth/register`, locked behind HIVE_INTERNAL_TOKEN), and a check that cannot be answered is a retryable 503.
 *  The node's register endpoint is never called from the browser. */
export async function POST(req: NextRequest) {
  const { userId } = await auth();
  if (!userId) return NextResponse.json({ ok: false, error: "not signed in" }, { status: 401 });
  const body = await req.json().catch(() => ({}) as any);
  const team = (body?.team as string) || "personal";

  const access = await teamAccess(team, userId);
  if (access === "unavailable") {
    return NextResponse.json({ ok: false, error: "membership check unavailable" }, { status: 503, headers: { "retry-after": "5" } });
  }
  if (access === "denied") return NextResponse.json({ ok: false, error: "not a member" }, { status: 403 });

  const headers: Record<string, string> = { "content-type": "application/json", "x-hive-team": team };
  if (TOKEN) headers["x-hive-internal"] = TOKEN;
  try {
    await fetch(`${ADMIN}/v1/zkauth/register`, { method: "POST", headers, body: JSON.stringify({ user_id: userId }) });
  } catch {}
  return NextResponse.json({ ok: true, team });
}
