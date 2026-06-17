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

export async function apiSend<T>(method: string, path: string, body?: unknown): Promise<T> {
  const r = await fetch(`${BASE}${path}`, {
    method,
    headers: { "content-type": "application/json", ...teamHeaders() },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!r.ok) throw new Error(`${method} ${path} -> ${r.status}`);
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
    // Re-fetch immediately when the active team changes.
    const onTeam = () => refresh();
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
  alias: string;
  state: "queued" | "building" | "ready" | "error";
  creator: string;
  git: GitSource | null;
  production: boolean;
  kind: string;
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
