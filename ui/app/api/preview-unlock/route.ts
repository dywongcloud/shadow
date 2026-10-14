import { NextResponse, type NextRequest } from "next/server";
import { auth } from "@clerk/nextjs/server";
import { sameOriginPath } from "@/lib/same-origin-path";
import { isPersonalTeam, teamAccess } from "@/lib/preview-access";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const TOKEN = process.env.HIVE_INTERNAL_TOKEN || "";
const NODE_TIMEOUT_MS = 10_000;
const LEGACY_PERSONAL_TEAM = "personal";
const UNTAGGED_TEAM = "__untagged__";
const DEPLOY_DOMAIN = (process.env.NEXT_PUBLIC_DEPLOYMENT_DOMAIN || "").trim().replace(/^\.+|\.+$/g, "");
const HOSTNAME = /^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)*[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/i;
const RETRY_AFTER_SECS = "5";

type TeamResolution = { team: string } | { refusal: NextResponse };

function isLocalhostHost(host: string): boolean {
  const h = host.toLowerCase();
  return h === "localhost" || h.endsWith(".localhost");
}

/** Whether `host` is a bare hostname the platform issues for deployments (the `.localhost` dev shapes and the NEXT_PUBLIC_DEPLOYMENT_DOMAIN apps wildcard):
 *  blocks an open redirect that would ship the minted proof to an arbitrary origin. A project's own CUSTOM domain is NOT matched, so a proof for one is refused here. */
function isKnownDeploymentHost(host: string): boolean {
  if (!HOSTNAME.test(host)) return false;
  const h = host.toLowerCase();
  if (isLocalhostHost(h)) return true;
  return !!DEPLOY_DOMAIN && (h === DEPLOY_DOMAIN || h.endsWith(`.${DEPLOY_DOMAIN}`));
}

/** Cause-specific copy for an unlock failure (an access-service fault is not "the deployment may still be starting"). */
function unlockError(status: number, stage: "enroll" | "mint"): string {
  if (status === 403) return "This platform rejected the unlock request. The dashboard is not authorized to grant preview access.";
  if (status === 404) return "The preview access service is unavailable on this node. Please try again.";
  if (status >= 500) return "The preview access service failed while verifying your membership. Please try again.";
  return `Could not ${stage === "enroll" ? "enroll you for" : "grant"} preview access (${status}).`;
}

function retryable(error: string): NextResponse {
  return NextResponse.json({ ok: false, error }, { status: 503, headers: { "retry-after": RETRY_AFTER_SECS } });
}

function nodeFetch(path: string, init: RequestInit = {}): Promise<Response> {
  return fetch(`${ADMIN}${path}`, { ...init, signal: AbortSignal.timeout(NODE_TIMEOUT_MS) });
}

/** The node's authoritative owning team of `project` (never the `?team=` the edge redirect carried): unanswerable is a retryable refusal, empty or untagged is
 *  the legacy `personal` namespace. */
async function resolveProjectTeam(project: string): Promise<TeamResolution> {
  let res: Response;
  try {
    res = await nodeFetch(`/v1/zkauth/project-team?project=${encodeURIComponent(project)}`, { headers: TOKEN ? { "x-hive-internal": TOKEN } : {} });
  } catch {
    return { refusal: retryable(unlockError(503, "enroll")) };
  }
  if (res.status === 403) {
    console.error(`[preview-unlock] project-team resolution refused (403) for ${project}; check HIVE_INTERNAL_TOKEN`);
    return { refusal: NextResponse.json({ ok: false, error: unlockError(403, "enroll") }, { status: 502 }) };
  }
  if (!res.ok) {
    console.error(`[preview-unlock] project-team resolution failed ${res.status} for ${project}`);
    const error = unlockError(res.status, "enroll");
    return { refusal: res.status >= 500 ? retryable(error) : NextResponse.json({ ok: false, error }, { status: 502 }) };
  }
  const body = await res.json().catch(() => null);
  if (!body || typeof body !== "object") return { refusal: retryable(unlockError(503, "enroll")) };
  const resolved = String(body.team ?? "").trim();
  return { team: resolved && resolved !== UNTAGGED_TEAM ? resolved : LEGACY_PERSONAL_TEAM };
}

/** Authorizes only from the node's project -> team answer plus the caller's Clerk identity, failing closed and retryable when either cannot be asked, then
 *  mints the preview proof for that team. */
export async function GET(req: NextRequest) {
  const sp = req.nextUrl.searchParams;
  const host = sp.get("host") || "";
  const project = sp.get("project") || "";
  const next = sameOriginPath(sp.get("next"), "/");
  if (!host || !project) return NextResponse.json({ ok: false, error: "missing deployment info" }, { status: 400 });
  if (!isKnownDeploymentHost(host)) {
    return NextResponse.json({ ok: false, error: "unrecognized deployment host" }, { status: 400 });
  }

  const { userId } = await auth();
  if (!userId) return NextResponse.json({ ok: false, signin: true, error: "sign in required" }, { status: 401 });

  const resolution = await resolveProjectTeam(project);
  if ("refusal" in resolution) return resolution.refusal;
  const { team } = resolution;

  const personal = isPersonalTeam(team);
  const access = await teamAccess(team, userId);
  if (access === "unavailable") return retryable("We could not verify your membership right now. Please try again.");
  if (access === "denied") {
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
    const reg = await nodeFetch("/v1/zkauth/register", { method: "POST", headers, body: JSON.stringify({ user_id: userId }) });
    if (!reg.ok) {
      const detail = await reg.text().catch(() => "");
      console.error(`[preview-unlock] member ${userId} of ${team}: enrollment failed ${reg.status}: ${detail}`);
      return NextResponse.json({ ok: false, error: unlockError(reg.status, "enroll") }, { status: 502 });
    }
    const r = await nodeFetch("/v1/zkauth/preview-proof", { method: "POST", headers, body: JSON.stringify({ user_id: userId, project, host }) });
    if (!r.ok) {
      const detail = await r.text().catch(() => "");
      console.error(`[preview-unlock] member ${userId} of ${team}: proof mint failed ${r.status}: ${detail}`);
      return NextResponse.json({ ok: false, error: unlockError(r.status, "mint") }, { status: 502 });
    }
    const d = await r.json();
    const scheme = isLocalhostHost(host) ? "http" : "https";
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
  } catch {
    return NextResponse.json({ ok: false, error: "unlock failed" }, { status: 502 });
  }
}
