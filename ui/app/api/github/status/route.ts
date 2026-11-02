import { NextResponse } from "next/server";
import { githubConnectionDetail, githubLastKnownDetail, resolveEntity } from "@/lib/github";

/** HONEST GitHub connection status: whether the connection is ACTIVE and live (Composio stays ACTIVE after GitHub revokes a token), plus the granted scopes, the account login and
 *  whether private-repo (`repo`) and org-enumeration (`read:org`) access are present, so the Integrations UI can show what a user can reach and prompt reconnect/approve. Never
 *  returns a token value.
 *
 *  The caller aborts this request after 4 s (`gitops.tsx`), so the handler bounds itself: past the deadline it answers from
 *  the stored credential alone with `degraded: true` and `reachability: "unknown"`, instead of the client catching an abort
 *  and rendering "not connected" for a connection that is fine. */
const STATUS_DEADLINE_MS = 3500;

export async function GET() {
  const entity = await resolveEntity();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<null>((res) => (timer = setTimeout(() => res(null), STATUS_DEADLINE_MS)));
  const detail = await Promise.race([githubConnectionDetail(entity), deadline]);
  if (timer) clearTimeout(timer);
  const body = detail ?? (await githubLastKnownDetail(entity));
  return NextResponse.json(body, { headers: { "cache-control": "no-store" } });
}
