import { NextRequest, NextResponse } from "next/server";
import {
  githubConfigured,
  githubConnectionDetail,
  githubInstallationDetail,
  githubLastKnownDetail,
  resolveEntity,
  type GithubDetail,
} from "@/lib/github";
import { getGithubAppTokenResult, type GithubCredential } from "@/lib/github-app";
import { authTokenFrom, backend } from "@/lib/gitops-server";

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

/** The tenant's linked config repo, from the SAME replicated store `/api/gitops/sync` reads. Only the
 *  ADDRESS is used: the sticky `connected` flag is never evidence. An anonymous answer is not data
 *  on an enforced node, so it never seeds an installation claim. */
async function linkedConfigRepo(req: NextRequest): Promise<{ owner: string; repo: string } | null> {
  const authToken = authTokenFrom(req);
  if (!authToken) return null;
  const team = req.headers.get("x-hive-team") || "personal";
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const read = backend("/v1/gitops", team, authToken);
    const bound = new Promise<null>((res) => (timer = setTimeout(() => res(null), 1600)));
    const r = await Promise.race([read, bound]);
    if (!r) return null;
    if (!r.ok || r.headers.get("x-hive-anon") === "1") return null;
    const link = await r.json().catch(() => null);
    const parts = (typeof link?.repo === "string" ? link.repo : "").split("/");
    if (parts.length !== 2 || !parts[0] || !parts[1]) return null;
    return { owner: parts[0], repo: parts[1] };
  } catch {
    return null;
  } finally {
    if (timer) clearTimeout(timer);
  }
}

export async function GET(req: NextRequest) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let cred: GithubCredential | undefined;
  let entity = "default";
  try {
    const deadline = new Promise<null>((res) => (timer = setTimeout(() => res(null), STATUS_DEADLINE_MS)));
    const detail = await Promise.race([
      (async () => {
        entity = await resolveEntity();
        cred = await getGithubAppTokenResult();
        const own = githubConnectionDetail(entity, cred, true).catch(() => null);
        // A phone that never authorised has NO browser grant ("none"), but the platform's GitHub
        // App installation is device-independent. Report it only when GitHub mints a token for THIS
        // tenant's linked config repo; never widen the grant cookie or persist a third-party bearer.
        const tenantToken = authTokenFrom(req);
        const installed =
          cred.state === "none" && tenantToken
            ? (async () => {
                const linked = await linkedConfigRepo(req);
                return linked ? githubInstallationDetail(entity, linked.owner, linked.repo).catch(() => null) : null;
              })().catch(() => null)
            : Promise.resolve(null);
        // The repo-scoped mint is bounded at 1.6 s. Await it FIRST: the platform-wide
        // /app/installations decoration in `own` can take 10 s and must never hold back an
        // answer GitHub has already given, nor eat the client's 6 s abort.
        const b = await installed;
        if (b) return b;
        const a = await own;
        if (a?.connected) return a;
        // With a tenant token, GitHub answered and granted nothing: "not connected" is real. With
        // NO token we cannot even ask about the tenant's link, so the answer is incomplete, not false.
        return tenantToken ? a : null;
      })().catch(() => null),
      deadline,
    ]);
    // A held grant degrades to its last known state. A browser with NO grant and an unfinished
    // check reports "unknown", never "not connected": the answer never arrived.
    const body =
      detail ??
      (cred?.state === "none" ? null : await githubLastKnownDetail(entity, cred).catch(() => null));
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
