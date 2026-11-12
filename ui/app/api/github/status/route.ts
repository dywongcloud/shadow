import { NextResponse } from "next/server";
import { githubConfigured, githubConnectionDetail, githubLastKnownDetail, resolveEntity, type GithubDetail } from "@/lib/github";
import { getGithubAppTokenResult, type GithubCredential } from "@/lib/github-app";

/** HONEST GitHub connection status: whether the connection is ACTIVE and live (Composio stays ACTIVE after GitHub revokes a token), plus the granted scopes, the account login and
 *  whether private-repo (`repo`) and org-enumeration (`read:org`) access are present, so the Integrations UI can show what a user can reach and prompt reconnect/approve. Never
 *  returns a token value.
 *
 *  The client aborts after 6 s (`gitops.tsx`), so the WHOLE chain is bounded at 3.5 s and answers
 *  from the SAME credential it already resolved. A second `bundleAuthorized` call used to fail
 *  closed on mobile Safari (Clerk dev-browser cookie withheld) and turn a valid connection into
 *  "not connected"; a bare race rejection used to become a 500 with the same false result. */
const STATUS_DEADLINE_MS = 3500;
const CHECK_INCOMPLETE: GithubDetail["credential"] = "unknown";

export async function GET() {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let cred: GithubCredential | undefined;
  let entity = "default";
  try {
    const deadline = new Promise<null>((res) => (timer = setTimeout(() => res(null), STATUS_DEADLINE_MS)));
    const detail = await Promise.race([
      (async () => {
        entity = await resolveEntity();
        cred = await getGithubAppTokenResult();
        return githubConnectionDetail(entity, cred, true);
      })().catch(() => null),
      deadline,
    ]);
    const body = detail ?? (await githubLastKnownDetail(entity, cred).catch(() => null));
    return NextResponse.json(
      body ?? {
        configured: githubConfigured(),
        connected: false,
        entity,
        login: null,
        scopes: [],
        hasPrivateAccess: false,
        hasOrgScope: false,
        live: false,
        credential: CHECK_INCOMPLETE,
        reachability: "unknown",
        degraded: true,
        reachabilityError: "The connection check did not complete — retry in a moment.",
      },
      { headers: { "cache-control": "no-store" } },
    );
  } finally {
    if (timer) clearTimeout(timer);
  }
}
