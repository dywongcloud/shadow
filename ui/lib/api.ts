"use client";

import { useEffect, useState, useCallback } from "react";

const BASE = "/cloud";

/** Current tenant (team) for scoping all dashboard reads/writes.
 *
 * MULTI-TENANCY: an ORG scope resolves to the org slug (already isolated). The
 * PERSONAL scope must NOT be the literal "personal" — that is a single global
 * namespace every account would share (the cross-account data-leak bug). Instead
 * it is keyed to the signed-in account's stable id (`hive_uid`, the Clerk user
 * id) → `u_<uid>`, so no two accounts ever collide on personal data.
 *
 * Before identity resolves, with Clerk enabled we return a namespace that owns NO
 * data (`__pending__`) rather than the shared "personal", so nothing can leak in
 * the pre-auth window; a `hive-team-changed` event fires when the id lands and
 * pollers re-fetch under the correct namespace. With Clerk disabled (local
 * single-user dev) plain "personal" is correct. */
const CLERK_ON = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;
// Dev-mint (bn-local-dev-clerk-hydration-gap): with the auth bypass on and no
// Clerk keys, /api/token mints a LOCAL JWT for the requested team, so local
// full-page testing exercises the real mint→cookie→JWT-tenant path instead of
// the anonymous/empty tenant. MINT_ON gates the same auto-mint paths CLERK_ON
// does (session keeper + 401/403 re-mint).
const DEV_MINT = process.env.NEXT_PUBLIC_HIVE_DEV_MINT === "1";
const MINT_ON = CLERK_ON || DEV_MINT;

export function currentTeam(): string {
  if (typeof window === "undefined") return "personal";
  const t = localStorage.getItem("hive_team");
  if (t && t !== "__personal__" && t !== "personal") return t; // org scope
  // Personal scope. The platform OWNER keeps the legacy "personal" namespace (so
  // their existing projects/deployments remain visible); every other account is
  // isolated under a per-user namespace. `hive_is_owner` is set from the backend's
  // authoritative owner_email check (identity/sync), never guessed client-side.
  if (localStorage.getItem("hive_is_owner") === "1") return "personal";
  const uid = localStorage.getItem("hive_uid");
  if (uid && uid !== "local") return `u_${uid}`;
  return CLERK_ON ? "__pending__" : "personal";
}

/** Platform-operator status of the current session, exactly as the backend
 *  derived it into the minted JWT (`platform_admin` claim, decoded by
 *  `/api/token` and mirrored to `hive_platform_admin` here). `null` = not yet
 *  known (pre-mint, or an older UI server that never reports the field) —
 *  callers must treat null as UNKNOWN and leave operator surfaces untouched,
 *  never as "not an operator". Re-reads on every `hive-session-changed`. */
export function usePlatformAdmin(): boolean | null {
  const [v, setV] = useState<boolean | null>(null);
  useEffect(() => {
    const read = () => {
      const s = localStorage.getItem("hive_platform_admin");
      setV(s === null ? null : s === "1");
    };
    read();
    window.addEventListener("hive-session-changed", read);
    return () => window.removeEventListener("hive-session-changed", read);
  }, []);
  return v;
}

function teamHeaders(): Record<string, string> {
  return { "x-hive-team": currentTeam() };
}

/**
 * Re-mint the httpOnly `hive_jwt` cookie for a specific team (defaults to the
 * current one). The backend now derives the tenant SOLELY from this cookie's
 * claim, so the cookie MUST carry the team the user is viewing — we pass it
 * explicitly (the route validates membership server-side) rather than relying on
 * Clerk's server-side active org, which lagged behind the client selection and
 * left every view stuck on the owner's "personal" tenant. Fire-and-forget safe:
 * awaiting lets callers sequence a re-fetch AFTER the new cookie is set.
 */
/** Resolved tenant the mint actually granted, or `null` on failure. A caller
 *  MUST treat `null` as "no valid session for this tenant exists" — never
 *  proceed as if the switch succeeded (that's what let a team-switch UI action
 *  broadcast `hive-team-changed` while the browser kept sending the OLD
 *  tenant's cookie, rendering that tenant's data under the new view).
 *
 *  Every call — automatic (the `ensureSessionMinted` gate, the 401/403
 *  re-mint) or explicit (team switch, the periodic re-mint intervals) — funnels
 *  through this ONE chokepoint, so `noteMintOutcome` sees every outcome and the
 *  failure budget below can never be bypassed by a caller minting directly.
 *  Explicit calls are never BLOCKED by that budget (a user action deserves a
 *  real attempt, and a success is how a degraded session recovers); only the
 *  automatic paths consult it before firing. */
export async function mintSessionToken(team?: string): Promise<string | null> {
  if (typeof window === "undefined") return null;
  const granted = await mintOnce(team);
  noteMintOutcome(granted);
  return granted;
}

async function mintOnce(team?: string): Promise<string | null> {
  const t = team ?? currentTeam();
  try {
    const r = await fetch("/api/token", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ team: t }),
    });
    // A failed mint (mint-failed-403 when the UI server is missing
    // HIVE_INTERNAL_TOKEN, mint-unreachable, rate-limited, …) leaves the
    // dashboard with no hive_jwt cookie at all — but critically the OLD
    // cookie (if any) is untouched and still valid, so silently continuing
    // as if this succeeded renders the PREVIOUS tenant's data under the view
    // the user just switched to. Don't let that be silent OR survivable.
    if (!r.ok) {
      const reason = await r.json().then((d) => d?.reason).catch(() => undefined);
      console.warn(`hive: session mint failed (${reason ?? `HTTP ${r.status}`}) — /cloud requests will be unauthenticated`);
      return null;
    }
    const d = (await r.json().catch(() => null)) as { ok?: boolean; tenant?: string; enforced?: boolean; platform_admin?: boolean } | null;
    if (!d?.ok) return null;
    // Mirror the backend-derived operator status for operator-only surfaces
    // (e.g. /cdn) so they can disclose the boundary instead of failing with a
    // bare 403. An ABSENT field (an older UI server mid-rollout) is UNKNOWN —
    // leave any previous flag untouched rather than locking an operator out.
    if (typeof d.platform_admin === "boolean") {
      localStorage.setItem("hive_platform_admin", d.platform_admin ? "1" : "0");
      window.dispatchEvent(new Event("hive-session-changed"));
    }
    // enforced:false = dev/no-Clerk backend: no cookie is set (unenforced), but
    // that is not a failure — every `/cloud` call is unauthenticated by design.
    if (d.enforced === false) return t;
    return d.tenant ?? t;
  } catch {
    /* dev/no-clerk or unreachable — the backend is unenforced there */
    return null;
  }
}

/**
 * Switch the active team: persist the selection, drop the previous tenant's
 * cached reads, RE-MINT the cookie for the new tenant, and only THEN broadcast
 * `hive-team-changed` so pollers re-fetch with the correct cookie already set (no
 * stale-tenant flash). This is the single correct entry point for changing teams.
 */
export async function switchTeam(slug: string): Promise<void> {
  if (typeof window === "undefined") return;
  const before = currentTeam(); // snapshot BEFORE the write, to restore on failure
  localStorage.setItem("hive_team", slug);
  invalidateApiCache();
  // Mint with the RESOLVED tenant (currentTeam() maps the "__personal__" sentinel
  // to "personal"/`u_<uid>`), so the cookie's claim matches what the backend scopes.
  const granted = await mintSessionToken();
  if (granted === null) {
    // The OLD cookie (whatever tenant it claimed) is still the only valid
    // session the browser holds. Broadcasting here would tell every poller
    // "the view is now `slug`" while `/cloud` keeps authenticating as the
    // PREVIOUS tenant — rendering that tenant's data under the new view. Undo
    // the localStorage write so `currentTeam()` still matches reality, and
    // surface the failure instead of silently mis-scoping every subsequent read.
    localStorage.setItem("hive_team", before);
    console.error(`hive: switch to "${slug}" failed to mint a session — view not changed; retry`);
    return;
  }
  // The backend can grant a DIFFERENT tenant than requested (e.g. the caller
  // isn't a member of the requested org — route.ts silently falls back to
  // their own personal namespace). Reconcile localStorage to what the COOKIE
  // actually claims so the UI's belief about the active tenant never diverges
  // from what reads are actually scoped to.
  if (granted !== currentTeam()) localStorage.setItem("hive_team", granted);
  window.dispatchEvent(new Event("hive-team-changed"));
}

/** fetch with a hard timeout so a slow/unreachable backend (e.g. a far region)
 *  can't leave a request — and the tab that awaits it — hanging indefinitely.
 *
 *  SESSION RESILIENCE: the `hive_jwt` cookie the backend authenticates every
 *  `/cloud` + `/ops` call against is minted with a 1-hour TTL and nothing used to
 *  refresh it mid-session — so after an hour EVERY request (a gitops link/sync,
 *  an env edit, a redeploy) 401'd and surfaced as a hard failure toast
 *  ("gitops sync failed · HTTP 401"). On a 401 we now transparently re-mint the
 *  cookie for the current tenant and retry the request ONCE; only a still-failing
 *  retry propagates. Same-origin `/api/token` is unaffected by the backend's auth
 *  (it authorizes via the server-side Clerk session), so the refresh works even
 *  when the old cookie is already rejected. Guarded to a single retry so a
 *  genuinely unauthorized call (not just an expired cookie) still fails fast. */
async function fetchWithTimeout(url: string, init: RequestInit, timeoutMs: number): Promise<Response> {
  // Wait for the mount-time session mint to at least have been attempted before
  // this request's FIRST try — see `ensureSessionMinted`'s doc comment below for
  // the exact race this closes. No-op after the first resolution.
  await ensureSessionMinted();
  const once = async () => {
    const ctrl = new AbortController();
    const t = setTimeout(() => ctrl.abort(), timeoutMs);
    try {
      return await fetch(url, { ...init, signal: ctrl.signal });
    } finally {
      clearTimeout(t);
    }
  };
  const r = await once();
  // 401 = expired/invalid session. 403 ALSO covers the no-cookie-at-all case:
  // the enforced ingress answers a missing hive_jwt with 403 (operator/tenant
  // guard), not 401 — e.g. right after sign-in before the first mint landed, or
  // when an earlier mint failed. Both get ONE transparent re-mint + retry; a
  // genuine authorization denial (wrong tenant/role) simply fails again on the
  // retry and propagates, so this cannot loop per-request. The cooldown keeps a
  // PERSISTENTLY-403 endpoint on a 3-4s poll from turning every cycle into an
  // /api/token round trip: one automatic re-mint per window is plenty (a
  // successful mint fixes every subsequent request anyway). This is an
  // AUTOMATIC mint path, so it also respects the shared failure budget: while
  // minting is in backoff or suspended (`failed`), re-minting here cannot
  // succeed any harder than it just did, so the response propagates as-is.
  if ((r.status === 401 || r.status === 403) && typeof window !== "undefined" && MINT_ON) {
    const now = Date.now();
    if (mintState === "failed" || now < mintNextAutoAttemptAt) return r;
    if (now - lastAutoMintAt < AUTO_MINT_COOLDOWN_MS) return r;
    lastAutoMintAt = now;
    const granted = await mintSessionToken();
    if (granted === null) return r; // re-mint failed — a retry would just re-fail
    return once();
  }
  return r;
}
let lastAutoMintAt = 0;
const AUTO_MINT_COOLDOWN_MS = 30_000;

// ---- Session-mint failure budget ------------------------------------------
// On a browser where the session can never persist (iOS/WebKit ITP blocking or
// purging the cookies Clerk's dev-instance handshake depends on, an unreachable
// backend, a UI server missing HIVE_INTERNAL_TOKEN), EVERY mint fails — and the
// pre-budget code re-fired a mint for every poll tick forever (witnessed live:
// ~24 POST /api/token per minute from the dashboard's four pollers, unbounded).
// Automatic mints now carry a consecutive-failure budget with escalating
// backoff; after MINT_MAX_CONSECUTIVE_FAILURES the mint enters an explicit
// terminal "failed" state and automatic paths stop retrying an unsatisfiable
// condition — requests simply proceed unauthenticated, exactly what they
// already did per-failure, minus the doomed /api/token round trip. The state is
// NOT permanent: any EXPLICIT mint (team switch, sign-in identity sync, the
// 4/50-minute re-mint intervals) still fires, and one success fully resets the
// budget. `sessionMintStatus()` + the `hive-session-mint-degraded` event let
// UI render the degraded state honestly instead of pretending auth is coming.
const MINT_MAX_CONSECUTIVE_FAILURES = 4;
const MINT_BACKOFF_MS = [2_000, 8_000, 30_000]; // after failure 1, 2, 3+
export type SessionMintState = "unattempted" | "ok" | "backoff" | "failed";
let mintState: SessionMintState = "unattempted";
let mintConsecutiveFailures = 0;
let mintNextAutoAttemptAt = 0; // epoch ms; automatic mints before this are skipped

/** Honest, renderable mint state: `failed` = automatic re-mints suspended after
 *  repeated failures (session cannot persist in this browser); reads degrade to
 *  unauthenticated rather than silently retrying forever. */
export function sessionMintStatus(): { state: SessionMintState; failures: number } {
  return { state: mintState, failures: mintConsecutiveFailures };
}

function noteMintOutcome(granted: string | null): void {
  if (granted !== null) {
    // Any success — automatic or explicit — fully recovers the budget.
    mintConsecutiveFailures = 0;
    mintNextAutoAttemptAt = 0;
    mintState = "ok";
    return;
  }
  mintConsecutiveFailures += 1;
  if (mintConsecutiveFailures >= MINT_MAX_CONSECUTIVE_FAILURES) {
    if (mintState !== "failed") {
      console.warn(
        `hive: session mint failed ${mintConsecutiveFailures}x in a row — suspending automatic re-mints (requests continue unauthenticated); a team switch, sign-in, or periodic re-mint can still recover`,
      );
      window.dispatchEvent(new Event("hive-session-mint-degraded"));
    }
    mintState = "failed";
  } else {
    mintState = "backoff";
    mintNextAutoAttemptAt =
      Date.now() + MINT_BACKOFF_MS[Math.min(mintConsecutiveFailures - 1, MINT_BACKOFF_MS.length - 1)];
  }
}

/**
 * Closes the mount-time race between `SessionToken`'s cookie mint and every
 * OTHER component's own data-fetch mount effect (`usePoll`, `NotificationBell`,
 * `PendingBuildsProvider`, …): both are independent sibling effects with no
 * ordering guarantee, so a data fetch could fire before the mint landed. When
 * the `hive_jwt` cookie has expired (1h TTL) but local identity is already
 * cached from a prior session (a RETURNING user, not a first-ever sign-in),
 * `currentTeam()` resolves synchronously and `usePoll`'s `__pending__` guard
 * never engages — the fetch goes out with no valid cookie. The backend's GET
 * path bypasses `require_auth`, so this returns a real 200 scoped to an
 * anonymous/empty tenant instead of a 401/403 — which is the ONLY thing that
 * triggers this file's existing auto-remint-and-retry below. The wrongly-scoped
 * empty response then gets committed as genuine settled data, producing a false
 * "Deploy your first project" empty state that no automatic poll tick ever
 * corrects (each new poll is scoped as unluckily as the first, and nothing ever
 * signals that the earlier 200 was wrong). Fix: every `/cloud`+`/ops` request
 * awaits ONE shared, idempotent mint promise before firing — whichever caller
 * (SessionToken's effect or a poll's own mount effect) reaches it FIRST kicks
 * off the mint; every other caller (this turn or a future one, until it
 * resolves) shares the same in-flight promise. Near-zero cost after the first
 * resolution (an already-settled promise). Resets to `null` on a failed mint so
 * a LATER caller (the next poll tick) can retry instead of wedging the gate shut
 * forever on one bad attempt.
 *
 * BOUNDED retry (the opposite failure): that reset-on-failure is what let a
 * browser where minting can NEVER succeed (mobile ITP purging the Clerk
 * session, dead backend) re-fire a doomed /api/token POST on every poll tick,
 * forever. Retries now go through the failure budget above: while in backoff
 * (or after the budget is exhausted → terminal `failed`) the gate resolves
 * immediately WITHOUT minting and the request proceeds unauthenticated — the
 * same outcome a failed mint already produced, minus the retry storm. The
 * original race-fix property is intact: whenever a mint is permitted, every
 * caller still awaits the ONE shared in-flight attempt before its first
 * request, and the first caller after a backoff window expires re-arms it.
 */
let sessionMintPromise: Promise<void> | null = null;
export function ensureSessionMinted(): Promise<void> {
  if (typeof window === "undefined" || !MINT_ON) return Promise.resolve();
  if (sessionMintPromise) return sessionMintPromise; // in-flight or settled-ok: share it
  if (mintState === "ok") return Promise.resolve(); // an explicit mint already succeeded
  if (mintState === "failed" || Date.now() < mintNextAutoAttemptAt) {
    return Promise.resolve(); // budget exhausted / backing off: degrade, don't re-fire
  }
  sessionMintPromise = mintSessionToken().then(
    (granted) => { if (granted === null) sessionMintPromise = null; },
    () => { sessionMintPromise = null; },
  );
  return sessionMintPromise;
}

// Short-TTL, tenant-scoped GET cache + in-flight de-dup. The dashboard has no
// client-side cache today: every click/poll/re-render that calls `apiGet` is a
// fresh network round trip, even when three things ask for the same path within
// the same second (command bar reopened, a poll burst, two components mounting
// together). A few seconds of staleness is invisible to a human clicking around;
// a network round trip on every interaction is not. Mutations (`apiSend`) clear
// the whole cache so a write is never masked by a stale read; the key includes
// `currentTeam()` so a team switch can never serve another tenant's cached data.
const GET_TTL_MS = 2000; // default: live-ish (overview, deployments, functions, nodes…)
const getCache = new Map<string, { at: number; data: unknown } | Promise<unknown>>();

// Per-path cache TTL. STABLE data (team lists, region/framework catalogs, project
// settings, billing/plans) rarely changes between navigations, so a long TTL
// collapses the many redundant polls (e.g. /v1/teams polled 4×, /v1/overview 6×)
// into one shared read — while LIVE data (logs, workflow runs, build status) keeps
// the short default so the tail still updates. Mutations invalidate the whole
// cache, so a longer TTL never masks a write the user just made. First matching
// prefix wins; longest-lived first.
const PATH_TTL: Array<[RegExp, number]> = [
  [/^\/v1\/regions(\/catalog)?$/, 10 * 60_000], // region catalog — effectively static
  [/^\/v1\/teams\/?$/, 60_000], // team list — changes only on create/leave (mutation invalidates)
  [/\/settings$/, 30_000], // project settings — edited via mutations (which invalidate)
  [/^\/v1\/billing/, 20_000], // account/plans/rate-card — slow-moving
  [/^\/v1\/apikeys/, 20_000],
  [/^\/v1\/domains/, 15_000],
  [/^\/v1\/workflows$/, 15_000], // workflow DEFINITIONS — change on deploy only (runs keep the live default)
  [/^\/v1\/ratelimit/, 15_000], // config — only this page's own mutations change it (which invalidate)
  [/^\/v1\/cluster$/, 5_000], // leadership/epoch — changes at gossip cadence, not per-second
  [/^\/v1\/nodes$/, 5_000], // mesh membership — same cadence
  [/^\/v1\/anycast$/, 5_000],
  // Admin Data Browser (collections/rows/namespaces + the Postgres/table view) —
  // an operator clicking between collections or re-opening the page doesn't need
  // a fresh network round trip every couple seconds; any edit made FROM this page
  // still invalidates immediately (opsSend clears the whole cache on every write).
  [/^\/v1\/admin\/data(\/.*)?$/, 15_000],
  [/^\/v1\/admin\/namespaces$/, 15_000],
  [/^\/v1\/admin\/sql\//, 15_000],
  // These previously fell back to the 2s GET_TTL_MS default, which is SHORTER
  // than every one of these paths' own poll interval (usage.tsx: 3s-300s
  // depending on Daily/Weekly/Monthly; observability.tsx: 5s; workflows.tsx:
  // 3s-12s) — so the cache barely de-duped anything; almost every poll tick
  // genuinely re-triggered the (often fleet-fan-out-backed) read. A short TTL
  // here collapses redundant reads from multiple co-mounted components/tabs
  // within the same window without masking any real staleness the page's own
  // interval wouldn't already tolerate.
  [/^\/v1\/metrics/, 3_000],
  [/^\/v1\/overview$/, 3_000],
  [/^\/v1\/functions$/, 3_000],
  [/^\/v1\/databases$/, 3_000],
  [/^\/v1\/integrations$/, 5_000],
  // Workflow RUNS list only (bare or `?summary=1`/`?project=`) — never the
  // single-run detail view (`/v1/workflows/runs/:id`), which a user actively
  // watching a trace wants at full freshness.
  [/^\/v1\/workflows\/runs(\?|$)/, 3_000],
  // Workflow HOOKS list — derived from world events; changes only as runs create
  // hooks, so a short de-dupe TTL is plenty (the Hooks tab polls slowly).
  [/^\/v1\/workflows\/hooks(\?|$)/, 8_000],
];
function pathTtl(path: string): number {
  for (const [re, ttl] of PATH_TTL) if (re.test(path)) return ttl;
  return GET_TTL_MS;
}

/** Drop every cached GET (called after any mutation, and on team switch). */
export function invalidateApiCache(): void {
  getCache.clear();
}
if (typeof window !== "undefined") {
  window.addEventListener("hive-team-changed", () => invalidateApiCache());
  // Cross-tab sync: the `storage` event fires in every OTHER tab (never the tab
  // that made the write) whenever `switchTeam` rewrites the shared
  // `hive_team` key. Without this, a second open tab keeps its stale
  // `usePoll`/`useOpsPoll` state (and, in the navbar, its stale `selected`
  // label) while `currentTeam()` — read fresh from localStorage on every call —
  // and the shared httpOnly cookie have ALREADY moved to the new tenant, so
  // that tab's next poll tick silently renders the new tenant's data under its
  // still-stale chrome. Re-dispatch the same-tab event locally so every
  // existing `hive-team-changed` listener (cache invalidation, usePoll,
  // useOpsPoll, the navbar's own selection) resyncs identically to a same-tab
  // switch — no bespoke per-component storage listener needed.
  window.addEventListener("storage", (e) => {
    if (e.key === "hive_team") window.dispatchEvent(new Event("hive-team-changed"));
  });
}

export async function apiGet<T>(path: string, opts?: { fresh?: boolean }): Promise<T> {
  const key = `${currentTeam()}|${path}`;
  const hit = getCache.get(key);
  // An in-flight request is always shared, `fresh` or not — it's the same data,
  // for the same tenant, at the same instant; there's no staleness cost, only a
  // saved duplicate round trip. Only a already-SETTLED cache entry is skipped
  // when `fresh` (used by usePoll's own interval ticks — a live-tail poll must
  // never be masked by a moments-old cached value from something else).
  if (hit instanceof Promise) return hit as Promise<T>;
  if (!opts?.fresh && hit && Date.now() - hit.at < pathTtl(path)) return hit.data as T;
  const req = (async () => {
    const r = await fetchWithTimeout(`${BASE}${path}`, { cache: "no-store", headers: teamHeaders() }, 20_000);
    if (!r.ok) throw new Error(`GET ${path} -> ${r.status}`);
    const data = (await r.json()) as T;
    getCache.set(key, { at: Date.now(), data });
    return data;
  })();
  getCache.set(key, req);
  req.catch(() => getCache.delete(key)); // never cache a failed lookup
  return req;
}

// Mutations to these paths change the declarative config, so they should reflect
// in the committed GitOps YAML via the server-side sync (there is no in-browser
// git fallback). We fire a debounced sync event the GitOps loop listens for. Covers:
// new projects + new deployments (`git/deploy`, `deploy/image`, `deploy/zip`),
// deleted projects + all settings/env/build/function/domain updates (`projects/`),
// teams, databases, securelinks. EXCLUDES `projects/:project/sandboxes/...` —
// sandboxes are ephemeral runtime state, not declarative config, and must never
// trigger a GitOps sync (the negative lookahead below carves that sub-resource
// out of the `projects\/` alternative).
const CONFIG_PATHS =
  /^\/v1\/(projects\/(?![^/]+\/sandboxes(?:\/|$))|teams\/?$|teams\/|databases|securelinks|gitops|git\/deploy|deploy\/(image|zip)|enterprise\/|routing|webhooks|domains|cron)/;
const GITOPS_BADGE = "background:#8957e5;color:#fff;padding:1px 5px;border-radius:3px;font-weight:600";

export async function apiSend<T>(method: string, path: string, body?: unknown): Promise<T> {
  const r = await fetchWithTimeout(`${BASE}${path}`, {
    method,
    headers: { "content-type": "application/json", ...teamHeaders() },
    body: body === undefined ? undefined : JSON.stringify(body),
  }, 30_000);
  if (!r.ok) {
    // Surface the server's error message (e.g. "project name already taken")
    // instead of an opaque status, so callers can show it to the user.
    const detail = await r.text().catch(() => "");
    throw new Error(detail?.trim() ? detail.trim() : `${method} ${path} -> ${r.status}`);
  }
  const isMutation = method !== "GET" && method !== "HEAD";
  if (isMutation) invalidateApiCache(); // a write must never be masked by a stale cached read
  if (typeof window !== "undefined" && isMutation && CONFIG_PATHS.test(path) && path !== "/v1/gitops") {
    // Reflect this CRUD op to GitOps via server-side sync (no in-browser git
    // fallback). Log it so the mirroring is observable in DevTools.
    const linked = localStorage.getItem("hive_gitops_linked") === "1";
    console.log(
      `%cgitops%c reflect ${method} ${path} → server sync (${linked ? "repo linked" : "not linked yet"})`,
      GITOPS_BADGE,
      "color:inherit",
    );
    window.dispatchEvent(new Event("gitops-sync"));
  }
  return r.json();
}

/**
 * PUT raw bytes to a binary endpoint (Drive file uploads, `/v1/drive/:project/file`).
 * `apiSend` always JSON-encodes its body, which would corrupt real file bytes —
 * this is the byte-body sibling: same auth/team header, same honest
 * server-message error surfacing, same write-invalidates-cache contract. Not
 * a `CONFIG_PATHS` match (drive files are runtime data, not declarative
 * config — the sandboxes carve-out precedent), so it never fires a GitOps
 * sync. A generous 120s timeout: uploads are the one write whose duration
 * scales with payload size, not with backend work.
 */
export async function apiPutBytes<T>(path: string, bytes: Blob | ArrayBuffer, contentType: string): Promise<T> {
  const r = await fetchWithTimeout(`${BASE}${path}`, {
    method: "PUT",
    headers: { "content-type": contentType || "application/octet-stream", ...teamHeaders() },
    body: bytes,
  }, 120_000);
  if (!r.ok) {
    const detail = await r.text().catch(() => "");
    throw new Error(detail?.trim() ? detail.trim() : `PUT ${path} -> ${r.status}`);
  }
  invalidateApiCache();
  return r.json();
}

/**
 * GET raw bytes from a binary endpoint (Drive file downloads). `apiGet`
 * always parses the response as JSON, which throws on a real file body —
 * this is the byte-body sibling, same auth/team header + error surfacing,
 * deliberately uncached (a `Blob` isn't the small JSON payload the shared
 * GET cache is sized for).
 */
export async function apiGetBytes(path: string): Promise<Blob> {
  const r = await fetchWithTimeout(`${BASE}${path}`, { headers: teamHeaders() }, 60_000);
  if (!r.ok) {
    const detail = await r.text().catch(() => "");
    throw new Error(detail?.trim() ? detail.trim() : `GET ${path} -> ${r.status}`);
  }
  return r.blob();
}

/**
 * POST a deploy through a same-origin Next server route (`/api/git/deploy` or
 * `/api/projects/:project/redeploy`) instead of straight to the backend. The
 * server route attaches the signed-in user's connected-GitHub token so the build
 * node can clone a PRIVATE repo, then forwards to the backend. Mirrors `apiSend`'s
 * error surfacing (throws the backend's message) + cache-invalidation + GitOps
 * reflection. The team rides both the body and the `x-hive-team` header.
 */
export async function apiDeployViaServerRoute<T>(routePath: string, body: Record<string, unknown>): Promise<T> {
  const r = await fetchWithTimeout(
    routePath,
    {
      method: "POST",
      headers: { "content-type": "application/json", ...teamHeaders() },
      body: JSON.stringify({ ...body, team: currentTeam() }),
    },
    60_000,
  );
  if (!r.ok) {
    const detail = await r.text().catch(() => "");
    throw new Error(detail?.trim() ? detail.trim() : `POST ${routePath} -> ${r.status}`);
  }
  invalidateApiCache();
  if (typeof window !== "undefined") window.dispatchEvent(new Event("gitops-sync"));
  return r.json();
}

/** Poll an endpoint on an interval. Returns {data, error, refresh, loading}.
 *  `active` (default true) gates the recurring interval — pass `false` to poll
 *  once (plus on team-change) and skip the background repeat, for data that's
 *  only worth refreshing while some UI affordance (a dropdown, a panel) is
 *  actually open. The initial fetch and team-change refresh always run either
 *  way, so the caller's data is never stale-forever. */
export function usePoll<T>(path: string, intervalMs = 3000, active = true, initial: T | null = null) {
  // `initial` seeds the first paint from a server component's prefetch, same
  // pattern and same reason as useOpsPoll's identical param: render real data
  // immediately instead of a blank/skeleton state while the first client poll
  // is in flight. The poll still fires normally on mount to pick up anything
  // that changed between the server fetch and hydration. Additive — every
  // existing call site omits this argument and gets `null`, i.e. today's
  // behavior unchanged.
  const [data, setData] = useState<T | null>(initial);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(initial === null);

  const load = useCallback(async (fresh: boolean) => {
    // Snapshot the tenant THIS request is for. `apiGet` resolves with data
    // authenticated by whichever cookie was attached when the fetch was SENT;
    // if the user switches tenants while this request is still in flight, a
    // slower response can resolve AFTER a newer, already-rendered request for
    // the NEW tenant — JS promises settle in completion order, not issue
    // order. Committing it unconditionally would silently overwrite the
    // correct view with the previous tenant's data (the leak: a stale
    // in-flight "test" org response clobbering the just-loaded "personal" list).
    const team = currentTeam();
    // Identity hasn't resolved yet (Clerk still loading / hive_uid not synced
    // into localStorage). Firing anyway would go out with no session cookie --
    // GETs bypass the backend's require_auth, so it still succeeds (200) but
    // scoped to whatever anonymous/default tenant the backend falls back to,
    // typically genuinely empty. That's a real (not-loading) empty response, so
    // it renders as a false "no projects yet" empty state on first load instead
    // of the loading skeleton -- exactly the leak __pending__ exists to avoid,
    // but only if callers actually skip the fetch instead of just tagging it.
    // Stay loading; the next interval tick or the hive-team-changed dispatch
    // (fired once identity resolves) retries.
    if (team === "__pending__") return;
    try {
      const d = await apiGet<T>(path, fresh ? { fresh: true } : undefined);
      if (currentTeam() !== team) return; // stale response for a tenant we've since left — discard
      setData(d);
      setError(null);
    } catch (e) {
      if (currentTeam() !== team) return;
      setError(String(e));
    } finally {
      if (currentTeam() === team) setLoading(false);
    }
  }, [path]);
  // Exposed to callers as a manual refresh (e.g. right after a mutation) — cache-
  // eligible is fine here since `apiSend` already clears the cache on any write.
  const refresh = useCallback(() => load(false), [load]);

  useEffect(() => {
    load(false); // initial mount: cache-eligible (helps cross-nav + co-mounted siblings)
    // Recurring ticks RESPECT the per-path TTL (load(false)): a live path (short
    // default TTL) still refreshes ~every tick, while a stable path (long TTL,
    // e.g. team list) is served from the shared cache — collapsing the many
    // redundant polls of the same endpoint into one network read per TTL window.
    // Mutations invalidate the cache, and refresh() below is available for
    // immediate post-write freshness, so nothing goes stale on a change.
    // Visibility gating: a hidden tab keeps ticking timers but skips the
    // actual fetch — a dashboard left open in a background tab was polling
    // every endpoint forever (per-tab, multiplying load browser → Next proxy →
    // backend). On return to visibility, one immediate refresh catches up.
    const tick = () => {
      if (typeof document !== "undefined" && document.visibilityState === "hidden") return;
      load(false);
    };
    const id = active ? setInterval(tick, intervalMs) : null;
    const onVisible = () => {
      if (typeof document !== "undefined" && document.visibilityState === "visible" && active) load(false);
    };
    if (typeof document !== "undefined") document.addEventListener("visibilitychange", onVisible);
    // On team/account switch (or logout), drop the previous tenant's data
    // IMMEDIATELY so it can never flash, then re-fetch for the new tenant.
    const onTeam = () => {
      setData(null);
      setError(null);
      setLoading(true);
      load(true);
    };
    if (typeof window !== "undefined") window.addEventListener("hive-team-changed", onTeam);
    return () => {
      if (id) clearInterval(id);
      if (typeof document !== "undefined") document.removeEventListener("visibilitychange", onVisible);
      if (typeof window !== "undefined") window.removeEventListener("hive-team-changed", onTeam);
    };
  }, [load, intervalMs, active]);

  return { data, error, loading, refresh };
}

// ---- Ops console (admin.shadw.cloud) ----
// The operator console reads through `/ops` (→ admin.shadw.cloud) instead of
// `/cloud` (→ api.shadw.cloud, the developer/API-key surface). Same request
// shape + tenant header; a separate base is the only difference. In-flight
// dedup/TTL cache is shared via the same key space (path includes the base).
const OPS_BASE = "/ops";

export async function opsGet<T>(path: string, opts?: { fresh?: boolean }): Promise<T> {
  const key = `${currentTeam()}|${OPS_BASE}${path}`;
  const hit = getCache.get(key);
  if (hit instanceof Promise) return hit as Promise<T>;
  if (!opts?.fresh && hit && Date.now() - hit.at < pathTtl(path)) return hit.data as T;
  const req = (async () => {
    const r = await fetchWithTimeout(`${OPS_BASE}${path}`, { cache: "no-store", headers: teamHeaders() }, 20_000);
    if (!r.ok) throw new Error(`GET ${path} -> ${r.status}`);
    const data = (await r.json()) as T;
    getCache.set(key, { at: Date.now(), data });
    return data;
  })();
  getCache.set(key, req);
  req.catch(() => getCache.delete(key));
  return req;
}

/** usePoll against the ops console host (`/ops` → admin.shadw.cloud). Identical
 *  semantics to [`usePoll`]; use it for the operator `/admin` pages. */
export function useOpsPoll<T>(path: string, intervalMs = 3000, active = true, initial: T | null = null) {
  // `initial` seeds the first paint from a server component's prefetch (see
  // lib/ops-data.ts's fetchOpsServer) so admin pages render real data
  // immediately instead of a blank/skeleton state while the first client poll
  // is in flight. The poll still fires normally on mount to pick up anything
  // that changed between the server fetch and hydration.
  const [data, setData] = useState<T | null>(initial);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(initial === null);

  const load = useCallback(async (fresh: boolean) => {
    // See usePoll's identical guard: discard a response that settles after the
    // active tenant has since changed, so a slow prior-tenant request can never
    // clobber the correctly-loaded new tenant's state.
    const team = currentTeam();
    // See usePoll's identical guard: skip firing while identity hasn't
    // resolved yet, so a false-empty unauthenticated response never lands.
    if (team === "__pending__") return;
    try {
      const d = await opsGet<T>(path, fresh ? { fresh: true } : undefined);
      if (currentTeam() !== team) return;
      setData(d);
      setError(null);
    } catch (e) {
      if (currentTeam() !== team) return;
      setError(String(e));
    } finally {
      if (currentTeam() === team) setLoading(false);
    }
  }, [path]);
  const refresh = useCallback(() => load(false), [load]);

  useEffect(() => {
    load(false);
    const id = active ? setInterval(() => load(false), intervalMs) : null; // respect per-path TTL
    const onTeam = () => { setData(null); setError(null); setLoading(true); load(true); };
    if (typeof window !== "undefined") window.addEventListener("hive-team-changed", onTeam);
    return () => {
      if (id) clearInterval(id);
      if (typeof window !== "undefined") window.removeEventListener("hive-team-changed", onTeam);
    };
  }, [load, intervalMs, active]);

  return { data, error, loading, refresh };
}

/** apiSend against the ops console host (`/ops` → admin.shadw.cloud). */
export async function opsSend<T>(method: string, path: string, body?: unknown): Promise<T> {
  const r = await fetchWithTimeout(`${OPS_BASE}${path}`, {
    method,
    headers: { "content-type": "application/json", ...teamHeaders() },
    body: body === undefined ? undefined : JSON.stringify(body),
  }, 30_000);
  if (!r.ok) {
    const detail = await r.text().catch(() => "");
    throw new Error(detail?.trim() ? detail.trim() : `${method} ${path} -> ${r.status}`);
  }
  if (method !== "GET" && method !== "HEAD") invalidateApiCache();
  return r.json();
}

// ---- shared types (mirror the Rust admin API) ----

export interface Overview {
  node: string;
  region: string;
  regions: string[];
  nodes: number;
  deployments: number;
  functions: number;
  instances: number;
  requests: number;
  blocked: number;
  cdn: { hits: number; misses: number; stale: number; entries: number; hit_ratio: number };
  concurrency: {
    plan: string;
    region: string;
    window_count: number;
    burst_limit: number;
    window_ms: number;
    max_concurrency: number;
    throttled_total: number;
  };
  waf_rules: number;
  waf_managed: boolean;
  cron_jobs: number;
  workflows: number;
}

export interface NodeInfo {
  id: string;
  name: string;
  region: string;
  public_url: string;
  peer_id: string | null;
  last_seen_ms: number;
  is_self: boolean;
  latency_ms?: number;
  healthy?: boolean;
  lat?: number | null;
  lon?: number | null;
  city?: string | null;
  country?: string | null;
  /** Static host capacity advertised by the node (gossiped). */
  cpu_cores?: number;
  mem_total_mb?: number;
  disk_total_gb?: number;
  /** GPUs driver-visible on the host (0 / absent = none). */
  gpu_count?: number;
  gpu_model?: string | null;
  gpu_vram_mb?: number;
  /** This node's own embedded iroh-relay listener URL — `null`/absent when
   *  unbound or the node has no reachable public address to advertise. */
  relay_url?: string | null;
  /** GuardianDB's own dialable iroh address (a separate identity from the
   *  request-routing mesh) — `null`/absent until that client has bound. */
  guardian_iroh_addr?: string | null;
}

export interface AnycastTable {
  region: string;
  selected: string | null;
  table: NodeInfo[];
  /** node name → count of deployments it actually hosts (real placement). A node
   *  with ≥1 is "serving"; 0/absent is "standby". */
  serving?: Record<string, number>;
}

export interface RateLimitStats {
  enabled: boolean;
  limit: number;
  window_ms: number;
  tracked_ips: number;
  blocked_total: number;
}

export interface Event {
  ts_ms: number;
  region: string;
  method: string;
  host: string;
  path: string;
  status: number;
  action: string;
  detail: string;
  project?: string;
  /** Exact deployment the request host aliased to at record time (empty when
   * the host resolved to no deployment) — scopes the deployment-detail logs. */
  deployment?: string;
}

export interface WafRule {
  id: string;
  description: string;
  action: "allow" | "deny" | "log";
  enabled: boolean;
  when: {
    path_regex?: string;
    method?: string;
    ip_prefix?: string;
    contains?: string;
  };
}

export interface CronJob {
  id: string;
  name: string;
  schedule: string;
  deployment: string;
  path: string;
  enabled: boolean;
  last_run_ms?: number;
  next_run_ms?: number;
  runs: number;
  /** "manual" (created here) or "vercel.json" (declared in a deployment's config). */
  source?: string;
}

export interface FunctionStats {
  key: string;
  instances: number;
  inflight: number;
  max_concurrency: number;
  requests: number;
  traditional_ms: number;
  fluid_ms: number;
  savings_pct: number;
  /** Active CPU pricing (Vercel Fluid convention). */
  active_cpu_ms: number;
  memory_gb_hrs: number;
  active_cpu_savings_pct: number;
  /** Scheduler observability. */
  safe_concurrency?: number;
  nack_total?: number;
  nack_concurrency?: number;
  nack_quota?: number;
  scale_out_total?: number;
  coldstart_deduped?: number;
  recycled?: number;
  last_provision_ms?: number;
  last_runtime_init_ms?: number;
}

export interface GitSource {
  repo_url: string;
  branch: string;
  commit: string;
  commit_message: string;
}

export interface Deployment {
  id: string;
  project: string;
  functions: string[];
  created_at_ms: number;
  /** Production-domain host alias `<project>.localhost`. */
  alias: string;
  /** Immutable per-commit host `<project>-<sha>.localhost` (Vercel commit URL). */
  commit_alias?: string;
  /** Per-branch host `<project>-git-<branch>.localhost` (latest on the branch). */
  branch_alias?: string;
  /** Immutable per-deployment host `<id>.localhost` (always this deployment). */
  id_alias?: string;
  /** "production" | "preview" (mirrors `production`). */
  target?: string;
  state: "queued" | "building" | "ready" | "error" | "cancelled";
  creator: string;
  git: GitSource | null;
  production: boolean;
  kind: string;
  /** Build framework slug (nextjs/vite/astro/docker/…) for the project-grid
   *  logo. Empty/absent = unknown → the card falls back to the default mark. */
  framework?: string;
  features?: DeploymentFeatures;
  /** Placement region (e.g. "san-jose") — the project's primary region, else default. */
  region?: string;
  /** Public ingress code for the region (iad/sin/sfo/lax); "" when unmapped. */
  region_code?: string;
  /** Stamped public raw TCP/UDP/gRPC port bindings (empty for HTTP-only
   *  deployments) — a raw-protocol service (Postgres, Minecraft, …) has no
   *  Host header to route on, so each gets its OWN public port here instead
   *  of the shared 80/443 gateway. See `ui/lib/deploy-url.ts`'s
   *  `rawPortAddress` for the ready-to-copy `host:port` connection string. */
  raw_ports?: RawPortBinding[];
  /** Browser-eligible functions with build-stamped artifact descriptors
   *  (`DeploymentInfo.browser_functions` — gossiped, so the merged
   *  /deployments list carries it for deployments hosted on other nodes too).
   *  Absent/empty for every deployment with no browser opt-in. */
  browser_functions?: BrowserFunctionRef[];
  /** Functions the build EVALUATED for browser eligibility and DECLINED, each
   *  with the reason (`DeploymentInfo.browser_ineligible` — gossiped alongside
   *  `browser_functions`, and the negative half of the same answer). The
   *  distinction that matters to a reader: a ready deployment with no artifact
   *  AND no entry here was never evaluated at all (built before the pipeline
   *  did this), so it needs a redeploy rather than a code change. Absent for
   *  pre-upgrade peers — which is exactly that same never-evaluated state. */
  browser_ineligible?: BrowserIneligibility[];
  /** The deployment's `browser_db` opt-in block, VERBATIM from the manifest
   *  (`DeploymentInfo.browser_db` — gossiped alongside `browser_functions`,
   *  raw policy resolved at the point of use). Absent for every deployment
   *  with no browser-database opt-in. A deployment can carry this WITHOUT any
   *  browser-eligible function — a Next.js/Express app with a `browser_db`
   *  block is exactly that — which is what makes a database-only browser node
   *  possible (see `lib/run-node-targets.ts`). */
  browser_db?: BrowserDbPolicy | null;
  /** This deployment's dedicated public IPv4 (the paid `dedicated_ipv4`
   *  addon), once purchased AND attached by a deploy — `DeploymentInfo.
   *  dedicated_ipv4`, gossiped fleet-wide for the same reason `raw_ports` is.
   *  Absent until a deploy after purchase actually stamps it — see
   *  `ProjectSettings.dedicated_ipv4`, which reflects the purchase itself and
   *  can be set here `undefined` (purchased, awaiting the next deploy). */
  dedicated_ipv4?: DedicatedIpv4;
}

/** A dedicated, static public IPv4 address (a real Tencent Cloud EIP)
 *  purchased for a project. Mirrors `fluid_core::DedicatedIpv4` field-for-
 *  field. */
export interface DedicatedIpv4 {
  address: string;
  tencent_eip_id: string;
  region: string;
  owner_node?: string;
  allocated_ms?: number;
}

/** One browser-eligible function of a deployment, with its build-stamped
 *  artifact descriptor. Mirrors `fluid_core::BrowserFunctionRef` — the
 *  artifact fields are serde-flattened into the same object. Descriptor
 *  metadata only; the artifact source bytes never leave the fleet. */
export interface BrowserFunctionRef {
  name: string;
  /** BLAKE3 hex of the emitted artifact source — never shown in the UI. */
  source_digest: string;
  /** BLAKE3 of the canonical policy encoding. THE wire digest the browser
   *  node's admission ties itself to — supplied to the worker from this
   *  descriptor, never typed by the donor. */
  policy_digest: string;
  /** Byte length of the emitted artifact source (UTF-8). */
  source_bytes: number;
  /** "quickjs" | "native" (fluid_core::BrowserExecMode, snake_case). */
  mode: string;
  timeout_ms: number;
  memory_bytes: number;
  stack_bytes: number;
  /** Sorted + deduplicated host-op ids (fluid_core::browser_host_op_abi). */
  allowed_ops: number[];
}

/** One function the build declined to make browser-servable, with the reason.
 *  Mirrors `fluid_core::BrowserIneligibility`. Entirely server-derived: the
 *  build clears any tenant-authored value before evaluating, so a fluid.json
 *  can never inject a reason of its own. */
export interface BrowserIneligibility {
  function: string;
  reason: string;
}

export interface RawPortBinding {
  public_port: number;
  function: string;
  container_port: number;
  /** "http" | "https" | "ws" | "wss" | "grpc" | "json-rpc" | "tcp" | "udp" */
  protocol: string;
  label?: string;
}

export interface DeploymentFeatures {
  redirects: number;
  rewrites: number;
  middleware: boolean;
  edge_functions: number;
  serverless_functions: number;
}

export interface EnvVar {
  key: string;
  value: string;
  target: string;
  /** Missing on legacy rows; the backend defaults those to runtime-only. */
  scope?: "runtime" | "build" | "all";
  sensitive: boolean;
  updated_ms: number;
}

export interface BuildConfig {
  framework: string;
  install_command: string;
  build_command: string;
  output_dir: string;
  root_dir: string;
}

export interface FunctionSettings {
  fluid_enabled: boolean;
  default_max_duration_secs: number;
  regions: string[];
  failover: boolean;
  /** vCPUs per instance (microVM). Standard tier = 1, Performance tier = 2. */
  vcpus: number;
  memory_mib: number;
  /** Serverless GPU: run this project's functions on GPU-equipped nodes with the host GPUs passed through. */
  gpu?: boolean;
}

export interface ProjectSettings {
  env: EnvVar[];
  build: BuildConfig;
  functions: FunctionSettings;
  domains?: string[];
  team?: string;
  preview_protection?: boolean;
  cron_enabled?: boolean;
  /** The Git branch whose deployments are production; "" until the project's
   *  first git deploy classifies it (or it has no git source at all). */
  production_branch?: string;
  /** Outcome of the auto-CI install attempted right after this project's first
   *  git import — absent for a project with no git source yet. */
  git_ci?: GitCiStatus;
  /** Dashboard-managed browser-replicated database opt-in (Storages page).
   *  Absent/null = not configured from the dashboard — a hand-edited
   *  fluid.json `browser_db` block can still be live even when this is null
   *  (this mirror only reflects what the dashboard itself last wrote, or what
   *  the last deploy synced back from an explicit fluid.json block). */
  browser_db?: BrowserDbPolicy | null;
  /** Dashboard-managed container config (resource ceilings + volume mount
   *  path) for a project running the `container` runtime. Only meaningful
   *  when the project's current deployment actually is one — see the
   *  Container settings page. Port/protocol are configured separately via
   *  the Network settings page (an immediate, no-redeploy live edit),
   *  not here. */
  container?: ContainerSettings | null;
  /** The project's paid dedicated public IPv4 allocation, if purchased
   *  (`hive_cloud::tencent_eip::provision_from_checkout`). `null`/absent
   *  until purchased; stays set across redeploys (the same address is
   *  re-adopted, never re-purchased). This is the PURCHASE record — it can
   *  be set here while a `Deployment.dedicated_ipv4` is still absent, which
   *  means "purchased, awaiting the next deploy to actually attach it". */
  dedicated_ipv4?: DedicatedIpv4 | null;
}

/** See `ProjectSettings.container`. Mirrors
 *  `hive_cloud::project_settings::ContainerSettings` field-for-field. */
export interface ContainerSettings {
  port?: number | null;
  protocol?: string | null;
  memory?: string | null;
  cpus?: string | null;
  pids?: number | null;
  volume_mount_path?: string | null;
}

// ---- Browser-replicated database (browser_db contract) ----
// Mirrors `fluid_core::BrowserDbPolicy` verbatim — presence is the opt-in.
export interface BrowserDbTable {
  /** `[A-Za-z_][A-Za-z0-9_]*` — validated both here and server-side. */
  name: string;
  /** Idempotent `CREATE TABLE IF NOT EXISTS ...`. */
  ddl: string;
}
export interface BrowserDbPolicy {
  /** 0 = platform default (64 MiB), ceiling 1 GiB. */
  max_bytes: number;
  /** 0 = platform default (1 MiB), ceiling 16 MiB. */
  max_value_bytes: number;
  /** Anonymous PUBLIC-scope donors get a read-only replica when true. */
  public_read: boolean;
  schema: BrowserDbTable[];
}
export const BROWSER_DB_MAX_BYTES_DEFAULT = 64 * 1024 * 1024;
export const BROWSER_DB_MAX_BYTES_MAX = 1024 * 1024 * 1024;
export const BROWSER_DB_VALUE_MAX_BYTES_DEFAULT = 1024 * 1024;
export const BROWSER_DB_VALUE_MAX_BYTES_MAX = 16 * 1024 * 1024;

/** `GET /v1/projects/:project/browser-db/status` response. */
export type BrowserDbStatusResponse =
  | { opted_in: false }
  | {
      opted_in: true;
      max_bytes: number;
      max_value_bytes: number;
      public_read: boolean;
      tables: string[];
      notes: string[];
      replica: {
        exists: boolean;
        bytes: number;
        sites: number;
        last_modified_ms: number | null;
      };
      /** The server-derived REST/Hrana surface (`browser_db_rest`). URLs come
       *  from the backend's own `api_base()` — never reconstructed client-side.
       *  `*_token_ms` disclose existence + mint time only; the plaintext token
       *  is shown once at mint and only its hash is stored server-side. */
      rest: {
        libsql_url: string;
        sql_url: string;
        team_token_ms: number | null;
        public_token_ms: number | null;
      };
    };

/** `POST /v1/projects/:project/browser-db/rest-token` response — the plaintext
 *  `token` is returned ONCE and never recoverable again (rotate to replace). */
export interface BrowserDbRestMint {
  project: string;
  scope: "team" | "public";
  token: string;
  libsql_url: string;
  sql_url: string;
}

/** Mint (rotating) a browser_db REST credential. `scope` "team" is read+write;
 *  "public" is read-only and mintable only while the policy has public_read. */
export function mintBrowserDbRestToken(project: string, scope: "team" | "public"): Promise<BrowserDbRestMint> {
  return apiSend<BrowserDbRestMint>(
    "POST",
    `/v1/projects/${encodeURIComponent(project)}/browser-db/rest-token`,
    { scope },
  );
}

/** See `ProjectSettings.git_ci`. Mirrors `hive-cloud::project_settings::GitCiStatus`. */
export interface GitCiStatus {
  webhook_installed: boolean;
  workflow_installed: boolean;
  /** Set only when neither installed — e.g. "github-not-connected", "bad-repo",
   *  "github-not-configured", "request-failed", "unknown". */
  skipped_reason: string;
  checked_ms: number;
}

export interface RegionEntry {
  id: string;
  label: string;
  aws: string;
  /** Real geographic coordinates of the region (from the node that backs it). */
  lat?: number | null;
  lon?: number | null;
  /** How many live nodes back this region. */
  nodes?: number;
}
export type RegionCatalog = Record<string, RegionEntry[]>;

// ---- Teams ----
export type Role = "owner" | "admin" | "member" | "viewer";
export interface Member {
  email: string;
  role: Role;
  name: string;
  added_ms: number;
}
export interface Team {
  slug: string;
  name: string;
  plan: string;
  created_ms: number;
  members: Member[];
  sso_enabled?: boolean;
}

// ---- Webhooks ----
export interface Webhook {
  id: string;
  project: string;
  url: string;
  events: string[];
  secret: string;
  enabled: boolean;
  created_ms: number;
}
export interface Delivery {
  id: string;
  webhook_id: string;
  event: string;
  url: string;
  status: number;
  ok: boolean;
  ts_ms: number;
  error: string;
}

// ---- Databases / storage ----
// `sqlite` is the MANAGED SQLite engine (`crates/hive-cloud/src/databases.rs`
// `DbKind::Sqlite`), spoken over libsql/Hrana with a real DSN. It is a
// different object from the browser-replicated cr-sqlite lane, which the
// storage page models as `browser_sqlite` and which has no wire endpoint.
export type DbKind =
  | "postgres" | "redis" | "blob" | "queue" | "vector" | "pubsub" | "realtime" | "sqlite" | "supabase";
export type DbStatus = "provisioning" | "ready" | "error";
export interface Database {
  id: string;
  name: string;
  project: string;
  team: string;
  kind: DbKind;
  region: string;
  status: DbStatus;
  provider?: string;
  mode: string;
  created_ms: number;
  /** Canonical database-gateway hostname and the node that owns its backing state. */
  db_host?: string;
  host_node?: string;
  connection: Record<string, string>;
  container: string | null;
  note: string;
  /** Configured replica regions (cross-region replication). */
  replicas?: string[];
  /** "primary" | "replica". */
  role?: string;
  /** Node hosting the primary (for replicas). */
  primary_node?: string;
}

// ---- Sandboxes ----
export type SandboxStatus = "pending" | "running" | "stopping" | "stopped" | "failed";
export type SandboxRuntime = "node26" | "node24" | "node22" | "python3.13";
export interface NetworkPolicy {
  mode: "allow-all" | "deny-all" | "allowlist";
  allowed_domains: string[];
  allowed_subnets: string[];
  denied_subnets: string[];
  forward_proxy?: string | null;
}
export interface EnvRef {
  key: string;
  value_enc: string;
}
export interface SandboxRecord {
  id: string;
  tenant_id: string;
  project_id: string;
  provider: string;
  provider_sandbox_name: string;
  provider_sandbox_id?: string | null;
  name: string;
  status: SandboxStatus;
  runtime: string;
  region: string;
  vcpus: number;
  memory_mb: number;
  timeout_ms: number;
  timeout_expires_at?: number | null;
  persistent: boolean;
  ports: number[];
  exposed_domains: Record<number, string>;
  network_policy: NetworkPolicy;
  env_refs: EnvRef[];
  tags: string[];
  current_snapshot_id?: string | null;
  snapshot_expiration_ms?: number | null;
  keep_last_snapshots?: number | null;
  active_cpu_usage_ms: number;
  total_duration_ms: number;
  total_active_cpu_duration_ms: number;
  total_ingress_bytes: number;
  total_egress_bytes: number;
  last_started_at?: number | null;
  last_stopped_at?: number | null;
  created_at: number;
  updated_at: number;
  deleted_at?: number | null;
  container: string;
  note: string;
}
export type SandboxCommandStatus = "queued" | "running" | "exited" | "failed" | "killed";
export interface LogLine {
  ts_ms: number;
  line: string;
}
export interface SandboxCommandRecord {
  id: string;
  tenant_id: string;
  project_id: string;
  sandbox_id: string;
  provider_command_id?: string | null;
  cmd: string;
  args: string[];
  cwd: string;
  sudo: boolean;
  detached: boolean;
  status: SandboxCommandStatus;
  exit_code?: number | null;
  stdout: LogLine[];
  stderr: LogLine[];
  started_at?: number | null;
  finished_at?: number | null;
  created_at: number;
  updated_at: number;
}
export interface SandboxSnapshotRecord {
  id: string;
  sandbox_id: string;
  provider_snapshot_id: string;
  source_session_id?: string | null;
  status: string;
  size_bytes?: number | null;
  created_at: number;
  expires_at?: number | null;
}
export interface SandboxMountRecord {
  id: string;
  sandbox_id: string;
  mount_path: string;
  type: "drive" | "remote-fuse";
  mode: "read-only" | "read-write";
  provider: string;
  config_refs: Record<string, string>;
  status: string;
  note: string;
  created_at: number;
  updated_at: number;
}

// ---- Monitoring ----
export interface MetricBucket {
  t_ms: number;
  requests: number;
  errors: number;
  client_err: number;
  blocked: number;
  cache_hits: number;
  cache_miss: number;
}
export interface Metrics {
  series: MetricBucket[];
  totals: { requests: number; errors: number; blocked: number; error_rate: number; cache_hit_ratio: number };
  status_distribution: Record<string, number>;
  top_paths: { path: string; count: number }[];
  projects: { project: string; requests: number }[];
}

// ---- Speed Insights (RUM) — wire shapes for GET /v1/speed-insights. One
// source of truth: previously duplicated privately inside speed-insights.tsx.
export interface VitalPercentiles {
  fcp: number | null;
  lcp: number | null;
  cls: number | null;
  inp: number | null;
  ttfb: number | null;
}
export interface RouteScore {
  route: string;
  count: number;
  res: number | null;
  p75: VitalPercentiles;
}
export interface RumSummary {
  sample_count: number;
  res: number | null;
  p75: VitalPercentiles;
  p90: VitalPercentiles;
  p95: VitalPercentiles;
  p99: VitalPercentiles;
  routes: RouteScore[];
}

// ---- Incidents / ops ----
export type Severity = "minor" | "major" | "critical";
export type IncidentStatus = "investigating" | "identified" | "monitoring" | "resolved";
export interface IncidentUpdate {
  ts_ms: number;
  status: IncidentStatus;
  message: string;
}
export interface Incident {
  id: string;
  title: string;
  severity: Severity;
  status: IncidentStatus;
  affected: string[];
  created_ms: number;
  updated_ms: number;
  updates: IncidentUpdate[];
}
export interface AdminOverview {
  owner: string;
  teams: number;
  projects: number;
  deployments: number;
  databases: { total: number; live: number };
  nodes: number;
  regions: string[];
  instances: number;
  requests: number;
  blocked: number;
  error_rate_30m: number;
  incidents_open: number;
  cluster: { term: number; leader: string; is_leader: boolean; members: string[]; consensus: string };
  webhooks: number;
}

// ---- Platform API keys ----
export interface ApiKey {
  id: string;
  name: string;
  prefix: string;
  team: string;
  role: string;
  created_ms: number;
  last_used_ms: number;
  token?: string; // only present in the create response
}

// ---- Secure compute (private backend tunnels) ----
export interface SecureLink {
  id: string;
  target: string;
  local_addr: string;
  region: string;
  status: string;
  public_key: string;
  created_ms: number;
  expires_ms: number;
  team: string;
  project: string;
  env_var: string;
}

// ---- Workflows ----
export type RunStatus = "pending" | "running" | "succeeded" | "failed" | "cancelled";
export interface WorkflowStep {
  name: string;
  deployment: string;
  path: string;
}
export interface WdkGraphNode { id: string; type?: string; data?: { label?: string; nodeKind?: string } }
export interface WdkGraphEdge { id: string; source: string; target: string; type?: string }
export interface WorkflowDef {
  id: string;
  name: string;
  project: string;
  steps: WorkflowStep[];
  /** Present when ingested from a Vercel WDK manifest: the workflow's node/edge
   *  graph (React-Flow shape) for canvas visualization. */
  graph?: { nodes: WdkGraphNode[]; edges: WdkGraphEdge[] } | null;
}
export interface StepRun {
  name: string;
  status: RunStatus;
  output: string;
  started_ms: number;
  finished_ms: number | null;
}
export interface WorkflowRun {
  id: string;
  def_id: string;
  name: string;
  project: string;
  status: RunStatus;
  steps: StepRun[];
  started_ms: number;
  finished_ms: number | null;
  /** Deployment environment the run executed on ("production" | "preview"). */
  environment?: string;
  /** Native WDK fields carried alongside the internal shape (superset). */
  createdAt?: number;
  updatedAt?: number;
  workflowName?: string;
}
export interface WorkflowSummaryRow {
  project: string;
  created: number;
  completed: number;
  failed: number;
  active: number;
}
/** Result of a run operation (the console's 3-dots menu). */
export interface RunOpResult {
  op: string;
  runId: string;
  status?: string;
  /** replay: the newly-created run's id. */
  replayedFromRunId?: string;
  /** wakeup: number of active sleeps cancelled. */
  stoppedCount?: number;
  enqueued?: boolean;
  alreadyTerminal?: boolean;
}

/** Run a workflow-run operation against the project's world (server-side,
 *  host-routed over the mesh). `op` ∈ cancel | replay | reenqueue | wakeup.
 *  Mirrors the upstream console's cancelRun / recreateRun / reenqueueRun /
 *  wakeUpRun. */
export async function runOp(
  op: "cancel" | "replay" | "reenqueue" | "wakeup",
  runId: string,
  project: string,
  extra?: { cancelReason?: string },
): Promise<RunOpResult> {
  const r = await fetchWithTimeout(
    `${BASE}/v1/workflows/runs/${encodeURIComponent(runId)}/${op}`,
    {
      method: "POST",
      headers: { "content-type": "application/json", ...teamHeaders() },
      body: JSON.stringify({ project, ...(extra || {}) }),
    },
    40_000,
  );
  if (!r.ok) {
    const detail = await r.text().catch(() => "");
    throw new Error(detail?.trim() || `${op} -> ${r.status}`);
  }
  invalidateApiCache();
  return r.json();
}

/** A workflow hook (webhook / createHook trigger), derived from world events.
 *  Mirrors the upstream console's Hooks table. */
export interface WorkflowHook {
  hookId: string;
  runId: string;
  project: string;
  token?: string | null;
  isWebhook?: boolean;
  /** Number of times the hook has been received/invoked. */
  invocations: number;
  created_ms: number;
  metadata?: unknown;
}

// ---- Domains / DNS ----
export interface DnsRecord {
  id: string;
  name: string;
  type: string;
  value: string;
  ttl: number;
  priority?: number | null;
  comment: string;
  created_ms: number;
  system: boolean;
}
export interface SslCert {
  id: string;
  cns: string[];
  renewal: string;
  issued_ms: number;
  expires_ms: number;
  provider: string;
}
export interface DomainRecord {
  domain: string;
  tenant: string;
  registrar: string;
  renewal_price: string;
  auto_renew: boolean;
  cdn_active: boolean;
  created_ms: number;
  expires_ms: number;
  nameservers: string[];
  ssl: SslCert;
  records: DnsRecord[];
  /** Ownership-verification state for custom-domain attachment (null for
   *  DNS-only records and pre-verification attachments). */
  verify?: DomainVerify | null;
}
/** TXT-challenge ownership proof for a custom domain attach. The alias only
 *  activates once `_hive-verify.<domain>` answers `txt_value` — re-proven at
 *  every activation, never trusted from the UI. */
export interface DomainVerify {
  status: "pending" | "verified";
  txt_name: string;
  txt_value: string;
  project: string;
  created_ms: number;
  checked_ms: number;
  verified_ms: number;
  last_probe: string;
}
export interface DomainDetail {
  domain: DomainRecord;
  connected: { project: string; domain: string }[];
}

/** Attach result from `POST /v1/projects/:p/domains` — attached immediately
 *  (already verified) or pending with the TXT instructions. */
export interface DomainAttachResult {
  domain: string;
  project: string;
  attached: boolean;
  status: "pending" | "verified";
  verify?: DomainVerify;
  instructions?: string;
  probe?: string;
}

export function attachDomain(project: string, domain: string): Promise<DomainAttachResult> {
  return apiSend<DomainAttachResult>("POST", `/v1/projects/${encodeURIComponent(project)}/domains`, { domain });
}
export function verifyDomainNow(domain: string): Promise<DomainAttachResult> {
  return apiSend<DomainAttachResult>("POST", `/v1/domains/${encodeURIComponent(domain)}/verify`);
}
export function detachDomain(project: string, domain: string): Promise<{ detached: boolean }> {
  return apiSend("DELETE", `/v1/projects/${encodeURIComponent(project)}/domains/${encodeURIComponent(domain)}`);
}
export interface NsRoster {
  nameservers: string[];
  /** False while the peer-attested nameserver set is still converging —
   *  `nameservers` must not be presented to users until this is true. */
  delegation_ready?: boolean;
  edge_ipv4: string[];
  edge_ipv6: string[];
}
export function fetchNsRoster(): Promise<NsRoster> {
  return apiGet<NsRoster>("/v1/domains-nameserver-roster");
}

// ---- Billing & compute credits ----
export interface PlanSpec {
  id: string;
  name: string;
  price_cents: number;
  included_cents: number;
  overage: boolean;
  max_projects: number;
  max_members: number;
  features: string[];
}
export interface RateCard {
  active_cpu_hr_cents: number;
  mem_gb_hr_cents: number;
  requests_per_million_cents: number;
  waf_per_million_cents: number;
}
export interface PlanLimits {
  max_projects: number;
  max_members: number;
  max_duration_secs: number;
  allows_failover: boolean;
  allows_sso: boolean;
  projects_used: number;
  members_used: number;
  can_deploy: boolean;
}
export interface InvoiceLine {
  description: string;
  amount_cents: number;
}
export interface Invoice {
  id: string;
  number: string;
  tenant: string;
  plan: string;
  period_start_ms: number;
  period_end_ms: number;
  lines: InvoiceLine[];
  subtotal_cents: number;
  total_cents: number;
  status: string;
  created_ms: number;
}
export interface BillingAccount {
  tenant: string;
  plan: string;
  status: string;
  included_cents: number;
  used_cents: number;
  balance_cents: number;
  period_start_ms: number;
  period_end_ms: number;
  updated_ms: number;
}
export interface BillingInfo {
  account: BillingAccount;
  plans: PlanSpec[];
  rate_card?: RateCard;
  limits?: PlanLimits;
  current_invoice?: Invoice;
}
export interface LedgerEntry {
  id: string;
  tenant: string;
  ts_ms: number;
  kind: string;
  amount_cents: number;
  balance_after_cents: number;
  note: string;
}

// ---- Notifications (inbox bell) ----
export interface Notification {
  id: string;
  severity: "error" | "warning" | "info";
  category: string;
  project: string;
  environment: string;
  message: string;
  ts_ms: number;
  read: boolean;
  archived: boolean;
}
export interface NotificationFeed {
  unread: number;
  inbox: number;
  items: Notification[];
}

export interface BuildLogLine {
  ts_ms: number;
  line: string;
}
export interface Build {
  id: string;
  project: string;
  repo_url: string;
  branch: string;
  commit: string;
  commit_message: string;
  state: "queued" | "building" | "ready" | "error" | "cancelled";
  started_ms: number;
  finished_ms: number | null;
  deployment_id: string | null;
  alias: string | null;
  lines: BuildLogLine[];
}

/** Cancel an in-flight build (`POST /v1/builds/:id/cancel`) — stops the
 *  ACTUAL server-side build process (git clone / npm install / build
 *  command), not just a client-side no-op, and returns the updated record
 *  (state: "cancelled"). Idempotent: cancelling an already-terminal build is
 *  a no-op that just echoes the current record back. */
export async function cancelBuild(buildId: string): Promise<Build> {
  return apiSend<Build>("POST", `/v1/builds/${encodeURIComponent(buildId)}/cancel`);
}

// ---- CDN routing (dashboard-managed redirects/rewrites; `/v1/routing`) ----
export interface RouteRedirect {
  source: string;
  destination: string;
  status: number;
}
export interface RouteRewrite {
  source: string;
  destination: string;
}
export interface RoutingConfig {
  redirects: RouteRedirect[];
  rewrites: RouteRewrite[];
}
