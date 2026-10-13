"use client";

import { useEffect, useState, useCallback } from "react";

const BASE = "/cloud";

/** Base ws(s):// origin for websocket routes (the sandbox shell): Next rewrites cannot forward an Upgrade, so this
 *  dials the admin host directly: NEXT_PUBLIC_HIVE_WS_ADMIN if set, else `api.<apex>` of the dashboard host, else the local admin port. */
export function wsBase(): string {
  const configured = process.env.NEXT_PUBLIC_HIVE_WS_ADMIN;
  if (configured) return configured.replace(/\/$/, "");
  if (typeof window === "undefined") return "";
  const { protocol, hostname } = window.location;
  const wsProto = protocol === "https:" ? "wss:" : "ws:";
  if (hostname === "localhost" || hostname === "127.0.0.1") {
    return `${wsProto}//${hostname}:8786`;
  }
  const parts = hostname.split(".");
  const apiHost = parts.length > 2 ? `api.${parts.slice(-2).join(".")}` : `api.${hostname}`;
  return `${wsProto}//${apiHost}`;
}

const CLERK_ON = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;
const DEV_MINT = process.env.NEXT_PUBLIC_HIVE_DEV_MINT === "1";
const MINT_ON = CLERK_ON || DEV_MINT;

/** The tenant while identity is unresolved with Clerk on: a namespace that owns no data, never the shared "personal". */
export const PENDING_TENANT = "__pending__";

/** The active tenant scoping every dashboard read and write: an org slug, `u_<uid>` for a personal account (the legacy shared
 *  "personal" namespace only for the platform owner, `hive_is_owner`), or `PENDING_TENANT` before identity lands. */
export function currentTeam(): string {
  if (typeof window === "undefined") return "personal";
  const t = localStorage.getItem("hive_team");
  if (t && t !== "__personal__" && t !== "personal") return t;
  if (localStorage.getItem("hive_is_owner") === "1") return "personal";
  const uid = localStorage.getItem("hive_uid");
  if (uid && uid !== "local") return `u_${uid}`;
  return CLERK_ON ? PENDING_TENANT : "personal";
}

/** Backend-derived operator status of this session (the minted JWT's `platform_admin` claim, mirrored to
 *  `hive_platform_admin`): `null` = not yet known, which callers must treat as UNKNOWN, never as "not an operator". */
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

/** Re-mints the httpOnly `hive_jwt` cookie for `team` (default: current) and resolves to the tenant the mint actually
 *  granted, or `null` on failure: callers must treat `null` as "no valid session for this tenant". Every automatic and
 *  explicit mint funnels through here; only the automatic ones consult the failure budget. */
export async function mintSessionToken(team?: string): Promise<string | null> {
  if (typeof window === "undefined") return null;
  const granted = await mintOnce(team);
  noteMintOutcome(granted);
  return granted;
}

/** The browser's own Clerk session token (read from clerk-js in memory, never persisted): lets `/api/token` resolve the
 *  session when server-side `auth()` cannot (a Clerk dev instance missing its dev-browser cookie). */
async function clerkSessionToken(): Promise<string | null> {
  try {
    const clerk = (window as unknown as {
      Clerk?: { session?: { getToken?: () => Promise<string | null | undefined> } };
    }).Clerk;
    if (!clerk?.session?.getToken) return null;
    return (await clerk.session.getToken()) ?? null;
  } catch {
    return null;
  }
}

async function mintOnce(team?: string): Promise<string | null> {
  const t = team ?? currentTeam();
  try {
    const clerkTok = await clerkSessionToken();
    const r = await fetch("/api/token", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        ...(clerkTok ? { authorization: `Bearer ${clerkTok}` } : {}),
      },
      body: JSON.stringify({ team: t, ...(clerkTok ? { clerk_token: clerkTok } : {}) }),
    });
    if (!r.ok) {
      const reason = await r.json().then((d) => d?.reason).catch(() => undefined);
      console.warn(`hive: session mint failed (${reason ?? `HTTP ${r.status}`}) — /cloud requests will be unauthenticated`);
      return null;
    }
    const d = (await r.json().catch(() => null)) as { ok?: boolean; tenant?: string; enforced?: boolean; platform_admin?: boolean } | null;
    if (!d?.ok) return null;
    if (typeof d.platform_admin === "boolean") {
      localStorage.setItem("hive_platform_admin", d.platform_admin ? "1" : "0");
      window.dispatchEvent(new Event("hive-session-changed"));
    }
    const backendUnenforced = d.enforced === false;
    if (backendUnenforced) return t;
    return d.tenant ?? t;
  } catch {
    return null;
  }
}

/** Switches the active team: persist, drop cached reads, RE-MINT the cookie, and only then broadcast
 *  `hive-team-changed`; a failed mint restores the previous team. The one entry point for changing teams. */
export async function switchTeam(slug: string): Promise<void> {
  if (typeof window === "undefined") return;
  const previousTeam = currentTeam();
  localStorage.setItem("hive_team", slug);
  invalidateApiCache();
  const granted = await mintSessionToken();
  if (granted === null) {
    localStorage.setItem("hive_team", previousTeam);
    console.error(`hive: switch to "${slug}" failed to mint a session — view not changed; retry`);
    return;
  }
  if (granted !== currentTeam()) localStorage.setItem("hive_team", granted);
  window.dispatchEvent(new Event("hive-team-changed"));
}

const STALE_EPOCH_MARKER = "stale control-plane epoch";
const STALE_EPOCH_RETRY_DELAY_MS = 1500;

async function isStaleEpochRefusal(r: Response): Promise<boolean> {
  if (r.status !== 503) return false;
  const body = await r.clone().text().catch(() => "");
  return body.includes(STALE_EPOCH_MARKER);
}

/** Hard-timeout fetch behind every `/cloud` + `/ops` call: waits for the session mint, retries a stale-epoch 503 once after a
 *  short delay, and answers a 401, 403 or anonymous-tenant response (`x-hive-anon`, which a GET gets instead of a 401 when its
 *  cookie is missing) with ONE re-mint + retry, gated by a cooldown and the failure budget. */
async function fetchWithTimeout(url: string, init: RequestInit, timeoutMs: number): Promise<Response> {
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
  let r = await once();
  if (await isStaleEpochRefusal(r)) {
    await new Promise((resolve) => setTimeout(resolve, STALE_EPOCH_RETRY_DELAY_MS));
    r = await once();
  }
  const answeredAnonymously = r.headers.get("x-hive-anon") === "1";
  const sessionRejected = r.status === 401 || r.status === 403 || answeredAnonymously;
  if (sessionRejected && typeof window !== "undefined" && MINT_ON) {
    const now = Date.now();
    if (mintState === "failed" || now < autoMintNotBeforeMs) return r;
    if (now - lastAutoMintAt < AUTO_MINT_COOLDOWN_MS) return r;
    lastAutoMintAt = now;
    const granted = await mintSessionToken();
    if (granted === null) return r;
    return once();
  }
  return r;
}
let lastAutoMintAt = 0;
const AUTO_MINT_COOLDOWN_MS = 30_000;

const MINT_MAX_CONSECUTIVE_FAILURES = 4;
const MINT_BACKOFF_BY_FAILURE_MS = [2_000, 8_000, 30_000];
export type SessionMintState = "unattempted" | "ok" | "backoff" | "failed";
let mintState: SessionMintState = "unattempted";
let mintConsecutiveFailures = 0;
let autoMintNotBeforeMs = 0;

/** Renderable mint state: `failed` = automatic re-mints suspended after repeated failures (the session cannot persist
 *  in this browser); reads then go out unauthenticated instead of retrying forever. */
export function sessionMintStatus(): { state: SessionMintState; failures: number } {
  return { state: mintState, failures: mintConsecutiveFailures };
}

function noteMintOutcome(granted: string | null): void {
  if (granted !== null) {
    mintConsecutiveFailures = 0;
    autoMintNotBeforeMs = 0;
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
    autoMintNotBeforeMs =
      Date.now() + MINT_BACKOFF_BY_FAILURE_MS[Math.min(mintConsecutiveFailures - 1, MINT_BACKOFF_BY_FAILURE_MS.length - 1)];
  }
}

let sessionMintPromise: Promise<void> | null = null;

/** Gates every `/cloud` + `/ops` request on ONE shared in-flight session mint, so a returning user's first fetch cannot beat the
 *  cookie and settle an anonymous-tenant 200 as real data; resolves at once while minting is in backoff or `failed`, and a failed
 *  mint resets the gate so a later caller can retry. */
export function ensureSessionMinted(): Promise<void> {
  if (typeof window === "undefined" || !MINT_ON) return Promise.resolve();
  if (sessionMintPromise) return sessionMintPromise;
  if (mintState === "ok") return Promise.resolve();
  if (mintState === "failed" || Date.now() < autoMintNotBeforeMs) {
    return Promise.resolve();
  }
  sessionMintPromise = mintSessionToken().then(
    (granted) => { if (granted === null) sessionMintPromise = null; },
    () => { sessionMintPromise = null; },
  );
  return sessionMintPromise;
}

const DEFAULT_GET_TTL_MS = 2000;
const getCache = new Map<string, { at: number; data: unknown } | Promise<unknown>>();

/** GET cache key, tenant-scoped so a team switch can never serve another tenant's cached data. */
function tenantScopedKey(path: string): string {
  return `${currentTeam()}|${path}`;
}

/** Per-path GET cache TTL, first match wins: stable data (catalogs, settings, billing) long, live data (logs, workflow
 *  runs, build status) the short default; any mutation clears the whole cache. */
const PATH_TTL: Array<[RegExp, number]> = [
  [/^\/v1\/regions(\/catalog)?$/, 10 * 60_000],
  [/^\/v1\/teams\/?$/, 60_000],
  [/\/settings$/, 30_000],
  [/^\/v1\/billing/, 20_000],
  [/^\/v1\/apikeys/, 20_000],
  [/^\/v1\/domains/, 15_000],
  [/^\/v1\/workflows$/, 15_000],
  [/^\/v1\/ratelimit/, 15_000],
  [/^\/v1\/cluster$/, 5_000],
  [/^\/v1\/nodes$/, 5_000],
  [/^\/v1\/anycast$/, 5_000],
  [/^\/v1\/admin\/data(\/.*)?$/, 15_000],
  [/^\/v1\/admin\/namespaces$/, 15_000],
  [/^\/v1\/admin\/sql\//, 15_000],
  [/^\/v1\/metrics/, 3_000],
  [/^\/v1\/overview$/, 3_000],
  [/^\/v1\/admin\/overview$/, 5_000],
  [/^\/v1\/incidents(\?|$)/, 4_000],
  [/^\/v1\/status\/incidents$/, 5_000],
  [/^\/v1\/functions$/, 3_000],
  [/^\/v1\/databases$/, 3_000],
  [/^\/v1\/integrations$/, 5_000],
  [/^\/v1\/workflows\/runs(\?|$)/, 3_000],
  [/^\/v1\/workflows\/hooks(\?|$)/, 8_000],
];
function pathTtl(path: string): number {
  for (const [re, ttl] of PATH_TTL) if (re.test(path)) return ttl;
  return DEFAULT_GET_TTL_MS;
}

/** Drop every cached GET (called after any mutation, and on team switch). */
export function invalidateApiCache(): void {
  getCache.clear();
}
if (typeof window !== "undefined") {
  window.addEventListener("hive-team-changed", () => invalidateApiCache());
  const relayTeamSwitchFromOtherTab = (e: StorageEvent) => {
    if (e.key === "hive_team") window.dispatchEvent(new Event("hive-team-changed"));
  };
  window.addEventListener("storage", relayTeamSwitchFromOtherTab);
}

export async function apiGet<T>(path: string, opts?: { fresh?: boolean }): Promise<T> {
  const key = tenantScopedKey(path);
  const hit = getCache.get(key);
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
  req.catch(() => getCache.delete(key));
  return req;
}

/** Mutations that change declarative config and so reflect to GitOps through the server-side sync (a `gitops-sync`
 *  event); sandboxes are ephemeral runtime state and never trigger it. */
const CONFIG_PATHS =
  /^\/v1\/(projects\/(?![^/]+\/sandboxes(?:\/|$))|teams\/?$|teams\/|databases|securelinks|gitops|git\/deploy|deploy\/(image|zip)|enterprise\/|routing|webhooks|domains|cron)/;
const GITOPS_BADGE = "background:#8957e5;color:#fff;padding:1px 5px;border-radius:3px;font-weight:600";

/** A server refusal carrying the API's typed error envelope (`code` such as "PLAN_UPGRADE_REQUIRED"). */
export class ApiError extends Error {
  status: number;
  code: string;
  constructor(status: number, message: string, code = "") {
    super(message);
    this.status = status;
    this.code = code;
  }
}

export interface LogsPage {
  rows: Event[];
  /** Rows matched before the current page's offset/limit slice. */
  total: number;
  hasMore: boolean;
  /** Oldest event this node still holds — the real floor of the history. */
  oldestMs: number;
}

/** `/v1/logs` reader that also surfaces the pagination headers; rows stay a plain `Event[]`. */
export async function apiGetLogs(path: string): Promise<LogsPage> {
  const r = await fetchWithTimeout(`${BASE}${path}`, { cache: "no-store", headers: teamHeaders() }, 20_000);
  const body = (await r.json().catch(() => null)) as unknown;
  if (!r.ok) {
    const e = (body as { error?: { code?: string; message?: string } } | null)?.error;
    throw new ApiError(r.status, e?.message || `GET ${path} -> ${r.status}`, e?.code || "");
  }
  const num = (k: string) => Number(r.headers.get(k) || 0);
  return {
    rows: Array.isArray(body) ? (body as Event[]) : [],
    total: num("x-hive-logs-total"),
    hasMore: r.headers.get("x-hive-logs-has-more") === "1",
    oldestMs: num("x-hive-logs-oldest-ms"),
  };
}

export async function apiSend<T>(method: string, path: string, body?: unknown): Promise<T> {
  const r = await fetchWithTimeout(`${BASE}${path}`, {
    method,
    headers: { "content-type": "application/json", ...teamHeaders() },
    body: body === undefined ? undefined : JSON.stringify(body),
  }, 30_000);
  if (!r.ok) {
    const detail = await r.text().catch(() => "");
    throw new Error(detail?.trim() ? detail.trim() : `${method} ${path} -> ${r.status}`);
  }
  const isMutation = method !== "GET" && method !== "HEAD";
  if (isMutation) invalidateApiCache();
  if (typeof window !== "undefined" && isMutation && CONFIG_PATHS.test(path) && path !== "/v1/gitops") {
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

/** PUT raw bytes to a binary endpoint (Drive uploads): same auth/team header, error surfacing and cache invalidation as
 *  `apiSend`, never a GitOps sync; 120 s timeout because upload time scales with payload size. */
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

/** GET raw bytes from a binary endpoint (Drive downloads): uncached, same auth and error surfacing as `apiGet`. */
export async function apiGetBytes(path: string): Promise<Blob> {
  const r = await fetchWithTimeout(`${BASE}${path}`, { headers: teamHeaders() }, 60_000);
  if (!r.ok) {
    const detail = await r.text().catch(() => "");
    throw new Error(detail?.trim() ? detail.trim() : `GET ${path} -> ${r.status}`);
  }
  return r.blob();
}

/** POSTs a deploy through a same-origin Next server route (`/api/git/deploy`, `/api/projects/:project/redeploy`) that
 *  attaches the user's GitHub token so the build node can clone a PRIVATE repo; otherwise as `apiSend`. */
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

/** Polls `path` every `intervalMs`: `{data, error, refresh, loading}`. `active=false` fetches once (plus on team change)
 *  and skips the recurring tick; `initial` seeds the first paint from a server component's prefetch. */
export function usePoll<T>(path: string, intervalMs = 3000, active = true, initial: T | null = null) {
  const [data, setData] = useState<T | null>(initial);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(initial === null);

  const load = useCallback(async (fresh: boolean) => {
    const requestTenant = currentTeam();
    if (requestTenant === PENDING_TENANT) return;
    try {
      const d = await apiGet<T>(path, fresh ? { fresh: true } : undefined);
      if (currentTeam() !== requestTenant) return;
      setData(d);
      setError(null);
    } catch (e) {
      if (currentTeam() !== requestTenant) return;
      setError(String(e));
    } finally {
      if (currentTeam() === requestTenant) setLoading(false);
    }
  }, [path]);
  const refresh = useCallback(() => load(false), [load]);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    load(false);
    const tick = () => {
      if (typeof document !== "undefined" && document.visibilityState === "hidden") return;
      load(false);
    };
    const id = active ? setInterval(tick, intervalMs) : null;
    const onVisible = () => {
      if (typeof document !== "undefined" && document.visibilityState === "visible" && active) load(false);
    };
    if (typeof document !== "undefined") document.addEventListener("visibilitychange", onVisible);
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

const OPS_BASE = "/ops";

export async function opsGet<T>(path: string, opts?: { fresh?: boolean }): Promise<T> {
  const key = tenantScopedKey(`${OPS_BASE}${path}`);
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
  const [data, setData] = useState<T | null>(initial);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(initial === null);

  const load = useCallback(async (fresh: boolean) => {
    const requestTenant = currentTeam();
    if (requestTenant === PENDING_TENANT) return;
    try {
      const d = await opsGet<T>(path, fresh ? { fresh: true } : undefined);
      if (currentTeam() !== requestTenant) return;
      setData(d);
      setError(null);
    } catch (e) {
      if (currentTeam() !== requestTenant) return;
      setError(String(e));
    } finally {
      if (currentTeam() === requestTenant) setLoading(false);
    }
  }, [path]);
  const refresh = useCallback(() => load(false), [load]);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    load(false);
    const id = active ? setInterval(() => load(false), intervalMs) : null;
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
  /** Server-derived build environment ("production" | "preview"); empty when the event cannot be joined to a deployment. */
  env?: string;
  /** Derived: "static" | "function" | "fullstack". */
  resource?: string;
  /** Derived: "hit" | "miss" | "stale" | "store" | "revalidate". */
  cache?: string;
  /** Derived severity: "info" | "warning" | "error" | "fatal". */
  level?: string;
  request_id?: string;
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
  /** Public raw TCP/UDP/gRPC port bindings (empty for HTTP-only deployments): a raw service has no Host header to route
   *  on, so each binding gets its own public port. */
  raw_ports?: RawPortBinding[];
  /** Browser-eligible functions with build-stamped artifact descriptors (gossiped, so deployments hosted on other nodes
   *  carry them too); absent or empty without a browser opt-in. */
  browser_functions?: BrowserFunctionRef[];
  /** Functions the build declined to make browser-servable, with the reason; a ready deployment with neither an artifact nor
   *  an entry here was never evaluated (built before the pipeline existed) and needs a redeploy. Absent for pre-upgrade peers. */
  browser_ineligible?: BrowserIneligibility[];
  /** The deployment's `browser_db` opt-in block verbatim from the manifest (gossiped); it can exist without any
   *  browser-eligible function, which is what makes a database-only browser node possible. */
  browser_db?: BrowserDbPolicy | null;
  /** The dedicated public IPv4 once purchased AND attached by a deploy (gossiped); `undefined` while purchased but awaiting
   *  the next deploy (see `ProjectSettings.dedicated_ipv4`, which reflects the purchase itself). */
  dedicated_ipv4?: DedicatedIpv4;
}

/** A dedicated static public IPv4 (a Tencent Cloud EIP) purchased for a project; mirrors `fluid_core::DedicatedIpv4`. */
export interface DedicatedIpv4 {
  address: string;
  tencent_eip_id: string;
  region: string;
  owner_node?: string;
  allocated_ms?: number;
}

/** One browser-eligible function with its build-stamped artifact descriptor (serde-flattened into the same object);
 *  mirrors `fluid_core::BrowserFunctionRef`. Metadata only: the artifact source bytes never leave the fleet. */
export interface BrowserFunctionRef {
  name: string;
  /** BLAKE3 hex of the emitted artifact source — never shown in the UI. */
  source_digest: string;
  /** BLAKE3 of the canonical policy encoding: the wire digest the browser node's admission binds to, supplied from this
   *  descriptor and never typed by the donor. */
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

/** A function the build declined to make browser-servable, with the reason; mirrors `fluid_core::BrowserIneligibility`.
 *  Entirely server-derived: the build clears any tenant-authored value, so a fluid.json cannot inject a reason. */
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
  /** Dashboard-managed browser-replicated database opt-in (Storages page); null = not configured from the dashboard, though
   *  a hand-edited fluid.json `browser_db` block can still be live. */
  browser_db?: BrowserDbPolicy | null;
  /** Dashboard-managed container config (resource ceilings + volume mount path) for the `container` runtime; port and
   *  protocol are a live edit on the Network settings page instead. */
  container?: ContainerSettings | null;
  /** The project's purchased dedicated IPv4 (the purchase record: stays set across redeploys, and may be set while
   *  `Deployment.dedicated_ipv4` is still absent, meaning purchased and awaiting the next deploy). */
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

export interface BrowserDbTable {
  /** `[A-Za-z_][A-Za-z0-9_]*` — validated both here and server-side. */
  name: string;
  /** Idempotent `CREATE TABLE IF NOT EXISTS ...`. */
  ddl: string;
}
/** Mirrors `fluid_core::BrowserDbPolicy` verbatim: presence is the opt-in. */
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
      /** The server-derived REST/Hrana surface (`browser_db_rest`): URLs come from the backend's own `api_base()`, never rebuilt
       *  client-side; `*_token_ms` disclose existence + mint time only (the plaintext token is shown once at mint). */
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
  /** `operator` = declared by a person; `automated` = a reconcile loop's
   *  observed condition, which resolves itself once no longer observed. */
  origin?: "operator" | "automated";
  condition?: string;
  observed_ms?: number;
  expires_ms?: number;
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

export interface ApiKey {
  id: string;
  name: string;
  prefix: string;
  team: string;
  role: string;
  created_ms: number;
  last_used_ms: number;
  /** Present only in the create response. */
  token?: string;
}

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

/** Runs a workflow-run operation against the project's world (server-side, host-routed over the mesh); mirrors the
 *  upstream console's cancelRun / recreateRun / reenqueueRun / wakeUpRun. */
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
/** TXT-challenge proof for a custom-domain attach: the alias activates only once `_hive-verify.<domain>` answers
 *  `txt_value`, re-proven at every activation and never trusted from the UI. */
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
  stripe_customer: string;
  period_start_ms: number;
  period_end_ms: number;
  updated_ms: number;
}
export interface BillingInfo {
  account: BillingAccount;
  plans: PlanSpec[];
  stripe: boolean;
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
  /** Why the build reached `error` (absent on older backends and on success). */
  error?: string | null;
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

/** Cancels an in-flight build (`POST /v1/builds/:id/cancel`), stopping the real server-side process; idempotent: a
 *  terminal build just echoes its record back. */
export async function cancelBuild(buildId: string): Promise<Build> {
  return apiSend<Build>("POST", `/v1/builds/${encodeURIComponent(buildId)}/cancel`);
}

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
