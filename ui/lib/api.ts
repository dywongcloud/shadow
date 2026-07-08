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
export async function mintSessionToken(team?: string): Promise<void> {
  if (typeof window === "undefined") return;
  const t = team ?? currentTeam();
  try {
    const r = await fetch("/api/token", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ team: t }),
    });
    // A failed mint (mint-failed-403 when the UI server is missing
    // HIVE_INTERNAL_TOKEN, mint-unreachable, …) leaves the dashboard with no
    // hive_jwt cookie at all — every /cloud call then 403s and each page just
    // renders empty. Don't let that be silent: log the reason so the console
    // names the real fault instead of a wall of anonymous 403s.
    if (!r.ok) {
      const reason = await r.json().then((d) => d?.reason).catch(() => undefined);
      console.warn(`hive: session mint failed (${reason ?? `HTTP ${r.status}`}) — /cloud requests will be unauthenticated`);
    }
  } catch {
    /* dev/no-clerk or unreachable — the backend is unenforced there */
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
  localStorage.setItem("hive_team", slug);
  invalidateApiCache();
  // Mint with the RESOLVED tenant (currentTeam() maps the "__personal__" sentinel
  // to "personal"/`u_<uid>`), so the cookie's claim matches what the backend scopes.
  await mintSessionToken();
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
  // successful mint fixes every subsequent request anyway).
  if ((r.status === 401 || r.status === 403) && typeof window !== "undefined" && CLERK_ON) {
    const now = Date.now();
    if (now - lastAutoMintAt < AUTO_MINT_COOLDOWN_MS) return r;
    lastAutoMintAt = now;
    await mintSessionToken();
    return once();
  }
  return r;
}
let lastAutoMintAt = 0;
const AUTO_MINT_COOLDOWN_MS = 30_000;

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
// in the committed GitOps YAML (or the local in-browser provider when no repo is
// linked). We fire a debounced sync event the GitOps loop listens for. Covers:
// new projects + new deployments (`git/deploy`), deleted projects + all settings/
// env/build/function/domain updates (`projects/`), teams, databases, securelinks.
const CONFIG_PATHS =
  /^\/v1\/(projects\/|teams\/?$|teams\/|databases|securelinks|gitops|git\/deploy|enterprise\/|routing|webhooks|domains|cron)/;
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
    // Reflect this CRUD op to GitOps (remote repo if linked, else the local
    // in-browser provider). Log it so the mirroring is observable in DevTools.
    const linked = localStorage.getItem("hive_gitops_linked") === "1";
    console.log(
      `%cgitops%c reflect ${method} ${path} → ${linked ? "remote config repo" : "local in-browser provider"}`,
      GITOPS_BADGE,
      "color:inherit",
    );
    window.dispatchEvent(new Event("gitops-sync"));
  }
  return r.json();
}

/** Poll an endpoint on an interval. Returns {data, error, refresh, loading}.
 *  `active` (default true) gates the recurring interval — pass `false` to poll
 *  once (plus on team-change) and skip the background repeat, for data that's
 *  only worth refreshing while some UI affordance (a dropdown, a panel) is
 *  actually open. The initial fetch and team-change refresh always run either
 *  way, so the caller's data is never stale-forever. */
export function usePoll<T>(path: string, intervalMs = 3000, active = true) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async (fresh: boolean) => {
    try {
      const d = await apiGet<T>(path, fresh ? { fresh: true } : undefined);
      setData(d);
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
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
    const id = active ? setInterval(() => load(false), intervalMs) : null;
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
export function useOpsPoll<T>(path: string, intervalMs = 3000, active = true) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async (fresh: boolean) => {
    try {
      setData(await opsGet<T>(path, fresh ? { fresh: true } : undefined));
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
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
  state: "queued" | "building" | "ready" | "error";
  creator: string;
  git: GitSource | null;
  production: boolean;
  kind: string;
  features?: DeploymentFeatures;
  /** Placement region (e.g. "san-jose") — the project's primary region, else default. */
  region?: string;
  /** Public ingress code for the region (iad/sin/sfo/lax); "" when unmapped. */
  region_code?: string;
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
}

export interface ProjectSettings {
  env: EnvVar[];
  build: BuildConfig;
  functions: FunctionSettings;
  domains?: string[];
  team?: string;
  preview_protection?: boolean;
  cron_enabled?: boolean;
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
export type DbKind = "postgres" | "redis" | "blob" | "queue" | "vector" | "pubsub" | "realtime";
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
}
export interface WorkflowSummaryRow {
  project: string;
  created: number;
  completed: number;
  failed: number;
  active: number;
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
}
export interface DomainDetail {
  domain: DomainRecord;
  connected: { project: string; domain: string }[];
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
  state: "queued" | "building" | "ready" | "error";
  started_ms: number;
  finished_ms: number | null;
  deployment_id: string | null;
  alias: string | null;
  lines: BuildLogLine[];
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
