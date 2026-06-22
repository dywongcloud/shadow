"use client";

import { useEffect, useState, useCallback } from "react";

const BASE = "/cloud";

/** Current tenant (team) for scoping all dashboard reads/writes. */
export function currentTeam(): string {
  if (typeof window === "undefined") return "personal";
  const t = localStorage.getItem("hive_team");
  return !t || t === "__personal__" ? "personal" : t;
}

function teamHeaders(): Record<string, string> {
  return { "x-hive-team": currentTeam() };
}

export async function apiGet<T>(path: string): Promise<T> {
  const r = await fetch(`${BASE}${path}`, { cache: "no-store", headers: teamHeaders() });
  if (!r.ok) throw new Error(`GET ${path} -> ${r.status}`);
  return r.json();
}

// Mutations to these paths change the declarative config, so they should reflect
// in the committed GitOps YAML. We fire a debounced sync event the GitOps loop
// listens for (server-side it's a no-op when unlinked / nothing changed).
const CONFIG_PATHS = /^\/v1\/(projects\/|teams\/?$|teams\/|databases|securelinks|gitops)/;

export async function apiSend<T>(method: string, path: string, body?: unknown): Promise<T> {
  const r = await fetch(`${BASE}${path}`, {
    method,
    headers: { "content-type": "application/json", ...teamHeaders() },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!r.ok) throw new Error(`${method} ${path} -> ${r.status}`);
  const isMutation = method !== "GET" && method !== "HEAD";
  if (typeof window !== "undefined" && isMutation && CONFIG_PATHS.test(path) && path !== "/v1/gitops") {
    window.dispatchEvent(new Event("gitops-sync"));
  }
  return r.json();
}

/** Poll an endpoint on an interval. Returns {data, error, refresh, loading}. */
export function usePoll<T>(path: string, intervalMs = 3000) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    try {
      const d = await apiGet<T>(path);
      setData(d);
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [path]);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, intervalMs);
    // On team/account switch (or logout), drop the previous tenant's data
    // IMMEDIATELY so it can never flash, then re-fetch for the new tenant.
    const onTeam = () => {
      setData(null);
      setError(null);
      setLoading(true);
      refresh();
    };
    if (typeof window !== "undefined") window.addEventListener("hive-team-changed", onTeam);
    return () => {
      clearInterval(id);
      if (typeof window !== "undefined") window.removeEventListener("hive-team-changed", onTeam);
    };
  }, [refresh, intervalMs]);

  return { data, error, loading, refresh };
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
}

export interface AnycastTable {
  region: string;
  selected: string | null;
  table: NodeInfo[];
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
  memory_mib: number;
}

export interface ProjectSettings {
  env: EnvVar[];
  build: BuildConfig;
  functions: FunctionSettings;
  domains?: string[];
  team?: string;
  preview_protection?: boolean;
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
  features: string[];
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
