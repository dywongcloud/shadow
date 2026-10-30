"use client";

import { useCallback, useEffect, useMemo, useState, use } from "react";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { Search, Play, RotateCw, X, MapPin, ShieldCheck, Zap } from "lucide-react";
import { Triangle, Badge } from "@/components/ui";
import { apiGetLogs, ApiError, type Event, type LogsPage } from "@/lib/api";
import { cn } from "@/lib/utils";

function statusTone(s: number) {
  if (s === 0) return "text-secondary";
  if (s >= 500) return "text-red-500";
  if (s >= 400) return "text-amber-500";
  if (s >= 300) return "text-blue-500";
  return "text-green";
}
function fmtTime(ms: number) {
  const d = new Date(ms);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${d.toLocaleString("en-US", { month: "short", day: "2-digit" }).toUpperCase()} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 2).slice(0, 2)}`;
}

/** Timeline windows the server supports (`admin::logs_range_ms`); `pro: true` marks a window the API refuses on the hobby plan, still offered
 *  so the refusal is legible instead of a silently empty table. */
const RANGES: { value: string; label: string; pro?: boolean }[] = [
  { value: "30m", label: "Last 30 minutes" },
  { value: "1h", label: "Last hour" },
  { value: "24h", label: "Last 24 hours" },
  { value: "7d", label: "Last 7 days" },
  { value: "30d", label: "Last 30 days", pro: true },
];
const LEVELS = ["warning", "error", "fatal"] as const;
const METHODS = ["get", "post", "put", "patch", "delete", "head", "options"];
const CACHES = ["hit", "miss", "stale", "store", "revalidate"];
const ENVS = ["production", "preview"];
const RESOURCES = ["static", "function", "fullstack"];
const PAGE_SIZES = [100, 300, 1000];
/** The server clamps `offset` to `LOGS_LIMIT_MAX - limit` (`admin::logs`), so a larger offset is silently reduced and "Next" would re-return the
 *  rows on screen: the UI stops at the same bound. */
const LOGS_LIMIT_MAX = 1000;
const LIVE_TAIL_POLL_MS = 1500;
const HISTORY_POLL_MS = 5000;

/** Union of the server's vocabulary and the values actually present in the
 * current rows, so a dropdown always offers the enum AND only real values. */
function optionsFor(fixed: string[], seen: string[]) {
  const out = [...fixed];
  for (const v of seen) if (v && !out.includes(v)) out.push(v);
  return out;
}

export function ProjectLogs({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const searchParams = useSearchParams();
  const [events, setEvents] = useState<Event[]>([]);
  const [q, setQ] = useState(() => searchParams.get("q") ?? "");
  const [live, setLive] = useState(true);
  const [sel, setSel] = useState<Event | null>(null);
  const [selRequestId, setSelRequestId] = useState("");

  const [range, setRange] = useState("30m");
  const [levels, setLevels] = useState<string[]>([]);
  const [fResource, setFResource] = useState("");
  const [fEnv, setFEnv] = useState("");
  const [fRoute, setFRoute] = useState("");
  const [fStatus, setFStatus] = useState("");
  const [fHost, setFHost] = useState("");
  const [fMethod, setFMethod] = useState("");
  const [fCache, setFCache] = useState("");
  const [pageSize, setPageSize] = useState(300);
  const [page, setPage] = useState(0);
  const [total, setTotal] = useState(0);
  const [hasMore, setHasMore] = useState(false);
  const [oldestMs, setOldestMs] = useState(0);
  const [err, setErr] = useState<ApiError | null>(null);

  function selectEvent(e: Event) {
    setSel(e);
    // eslint-disable-next-line react-hooks/purity -- runs only from a click handler (never during render); generates a display-only synthetic id at selection time
    setSelRequestId(Math.random().toString(36).slice(2, 8));
  }

  const levelKey = levels.slice().sort().join(",");
  const filterKey = JSON.stringify([range, levelKey, fStatus, fMethod, fHost, fRoute, fCache, fEnv, fResource, q, pageSize]);
  const [pagedFilterKey, setPagedFilterKey] = useState(filterKey);
  if (pagedFilterKey !== filterKey) {
    setPagedFilterKey(filterKey);
    setPage(0);
  }
  const offset = page * pageSize;
  const maxOffset = Math.max(0, LOGS_LIMIT_MAX - pageSize);
  const nextOffset = (page + 1) * pageSize;
  const canNext = hasMore && nextOffset <= maxOffset;

  const load = useCallback(async () => {
    const qs = new URLSearchParams();
    qs.set("project", project);
    qs.set("limit", String(pageSize));
    qs.set("range", range);
    if (q.trim()) qs.set("q", q.trim());
    if (levelKey) qs.set("level", levelKey);
    if (fStatus) qs.set("status", fStatus);
    if (fMethod) qs.set("method", fMethod);
    if (fHost) qs.set("host", fHost);
    if (fRoute) qs.set("route", fRoute);
    if (fCache) qs.set("cache", fCache);
    if (fEnv) qs.set("env", fEnv);
    if (fResource) qs.set("resource", fResource);
    if (offset > 0) qs.set("offset", String(offset));
    try {
      const page0: LogsPage = await apiGetLogs(`/v1/logs?${qs.toString()}`);
      setEvents(page0.rows);
      setTotal(page0.total);
      setHasMore(page0.hasMore);
      setOldestMs(page0.oldestMs);
      setErr(null);
    } catch (e) {
      setErr(e instanceof ApiError ? e : new ApiError(0, String((e as Error)?.message || e)));
    }
  }, [project, q, range, levelKey, fStatus, fMethod, fHost, fRoute, fCache, fEnv, fResource, pageSize, offset]);

  const isLiveTailWindow = range === "30m" || range === "1h";
  const pollMs = isLiveTailWindow ? LIVE_TAIL_POLL_MS : HISTORY_POLL_MS;

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch-on-mount/poll; state is set only after an internal await, not synchronously
    load();
    if (!live) return;
    const id = setInterval(load, pollMs);
    return () => clearInterval(id);
  }, [load, live, pollMs]);

  const counts = useMemo(() => {
    const c: Record<string, number> = { info: 0, warning: 0, error: 0, fatal: 0 };
    for (const e of events) if (e.level) c[e.level] = (c[e.level] || 0) + 1;
    return c;
  }, [events]);

  const seen = useMemo(() => {
    const hosts = new Set<string>();
    const routes = new Set<string>();
    const statuses = new Set<string>();
    const methods = new Set<string>();
    const envs = new Set<string>();
    const resources = new Set<string>();
    const caches = new Set<string>();
    for (const e of events) {
      if (e.host) hosts.add(e.host);
      if (e.path) routes.add(e.path);
      if (e.status) statuses.add(String(e.status));
      if (e.method) methods.add(e.method.toLowerCase());
      if (e.env) envs.add(e.env);
      if (e.resource) resources.add(e.resource);
      if (e.cache) caches.add(e.cache);
    }
    for (const cls of ["2xx", "3xx", "4xx", "5xx"]) statuses.add(cls);
    return {
      hosts: [...hosts].sort(),
      routes: [...routes].sort().slice(0, 100),
      statuses: [...statuses].sort(),
      methods: [...methods].sort(),
      envs: [...envs].sort(),
      resources: [...resources].sort(),
      caches: [...caches].sort(),
    };
  }, [events]);

  const upgrade = err?.code === "PLAN_UPGRADE_REQUIRED";

  return (
    <div>
      <div className="mb-4 flex items-center gap-3">
        <Triangle />
        <div className="flex items-center gap-2">
          <span className="h-2 w-2 rounded-full bg-green" />
          <Link href={`/projects/${encodeURIComponent(project)}`} className="text-lg font-semibold hover:underline">{project}</Link>
          <span className="text-muted">/</span>
          <span className="text-lg font-semibold">Logs</span>
        </div>
      </div>

      {err && (
        <div className={cn("mb-3 rounded-lg border px-3 py-2 text-sm", upgrade ? "border-amber-500/40 bg-amber-500/10 text-amber-600" : "border-red-500/40 bg-red-500/10 text-red-500")}>
          <span className="font-medium">{upgrade ? "Upgrade required" : "Could not load logs"}</span>
          <span className="text-secondary"> — {err.message}</span>
        </div>
      )}

      <div className="grid grid-cols-1 gap-0 overflow-hidden rounded-xl border border-border lg:grid-cols-[220px_1fr]">
        <aside className="border-b border-border bg-subtle/40 p-4 lg:border-b-0 lg:border-r">
          <div className="mb-3 flex items-center justify-between">
            <span className="text-sm font-semibold">Filters</span>
            <button
              onClick={() => {
                setQ("");
                setLevels([]);
                setFResource("");
                setFEnv("");
                setFRoute("");
                setFStatus("");
                setFHost("");
                setFMethod("");
                setFCache("");
                setPage(0);
              }}
              className="text-xs text-secondary hover:text-fg"
            >
              Reset
            </button>
          </div>
          <div className="mb-4">
            <div className="mb-1.5 text-xs font-medium text-secondary">Timeline</div>
            <select
              value={range}
              onChange={(e) => setRange(e.target.value)}
              className="w-full rounded-md border border-border bg-card px-2 py-1.5 text-xs focus:outline-none"
            >
              {RANGES.map((r) => (
                <option key={r.value} value={r.value}>
                  {r.label}
                  {r.pro ? " — Pro" : ""}
                </option>
              ))}
            </select>
            {oldestMs > 0 && (
              <div className="mt-1 text-[10px] text-muted">
                history back to {new Date(oldestMs).toLocaleString()}
              </div>
            )}
          </div>
          <div className="mb-2 text-xs font-medium text-secondary">Console Level</div>
          {LEVELS.map((l) => (
            <Level
              key={l}
              label={l[0].toUpperCase() + l.slice(1)}
              n={counts[l] || 0}
              checked={levels.includes(l)}
              onChange={(on) => setLevels((prev) => (on ? [...prev, l] : prev.filter((x) => x !== l)))}
            />
          ))}
          <div className="mt-3 flex flex-col gap-0.5 text-xs text-secondary">
            <FilterRow label="Resource" value={fResource} options={optionsFor(RESOURCES, seen.resources)} onChange={setFResource} />
            <FilterRow label="Environment" value={fEnv} options={optionsFor(ENVS, seen.envs)} onChange={setFEnv} />
            <FilterRow label="Route" value={fRoute} options={seen.routes} onChange={setFRoute} />
            <FilterRow label="Status Code" value={fStatus} options={seen.statuses} onChange={setFStatus} />
            <FilterRow label="Host" value={fHost} options={seen.hosts} onChange={setFHost} />
            <FilterRow label="Request Method" value={fMethod} options={optionsFor(METHODS, seen.methods)} onChange={setFMethod} />
            <FilterRow label="Cache" value={fCache} options={optionsFor(CACHES, seen.caches)} onChange={setFCache} />
          </div>
        </aside>

        <div className="min-w-0">
          <div className="flex items-center gap-2 border-b border-border p-3">
            <div className="relative flex-1">
              <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
              <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search logs…" className="w-full rounded-md border border-border bg-card py-1.5 pl-9 pr-2 text-sm focus:outline-none" />
            </div>
            <button onClick={() => setLive(!live)} className={cn("flex items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-sm", live ? "border-green/40 text-green" : "border-border text-secondary")}>
              <Play className="h-3.5 w-3.5" /> {live ? "Live" : "Paused"}
            </button>
            <button onClick={load} className="rounded-md border border-border p-1.5 text-secondary hover:bg-subtle"><RotateCw className="h-4 w-4" /></button>
          </div>

          <div className="flex">
            <div className="min-w-0 flex-1 overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="text-xs uppercase tracking-wide text-muted">
                    <th className="px-3 py-2 text-left font-medium">Time</th>
                    <th className="px-3 py-2 text-left font-medium">Status</th>
                    <th className="px-3 py-2 text-left font-medium">Host</th>
                    <th className="px-3 py-2 text-left font-medium">Request</th>
                  </tr>
                </thead>
                <tbody className="font-mono text-xs">
                  {events.map((e, i) => (
                    <tr key={i} onClick={() => selectEvent(e)} className={cn("cursor-pointer border-t border-border hover:bg-subtle", sel === e && "bg-subtle")}>
                      <td className="whitespace-nowrap px-3 py-2 text-secondary">{fmtTime(e.ts_ms)}</td>
                      <td className={cn("px-3 py-2", statusTone(e.status))}>{e.method} {e.status || "---"}</td>
                      <td className="max-w-[200px] truncate px-3 py-2 text-secondary">{e.host}</td>
                      <td className="max-w-[280px] truncate px-3 py-2">{e.path}</td>
                    </tr>
                  ))}
                  {!events.length && !err && <tr><td colSpan={4} className="px-3 py-10 text-center text-sm text-secondary">No logs match these filters.</td></tr>}
                </tbody>
              </table>
              <div className="flex items-center justify-between gap-3 border-t border-border px-3 py-2 text-xs text-secondary">
                <span>
                  {events.length
                    ? `${offset + 1}–${offset + events.length} of ${total}${hasMore ? "+" : ""}`
                    : `0 of ${total || 0}`}{" "}
                  matched
                </span>
                <span className="flex items-center gap-2">
                  <label className="flex items-center gap-1">
                    rows
                    <select value={pageSize} onChange={(e) => setPageSize(Number(e.target.value))} className="rounded-md border border-border bg-card px-1.5 py-1 text-xs focus:outline-none">
                      {PAGE_SIZES.map((n) => <option key={n} value={n}>{n}</option>)}
                    </select>
                  </label>
                  <button disabled={page === 0} onClick={() => setPage((p) => Math.max(0, p - 1))} className="rounded-md border border-border px-2 py-1 disabled:opacity-40">Prev</button>
                  <span>page {page + 1}</span>
                  <button
                    disabled={!canNext}
                    title={
                      hasMore && !canNext
                        ? `The API returns at most ${LOGS_LIMIT_MAX} rows per query — narrow the range or filters to reach older rows.`
                        : undefined
                    }
                    onClick={() => setPage((p) => p + 1)}
                    className="rounded-md border border-border px-2 py-1 disabled:opacity-40"
                  >
                    Next
                  </button>
                </span>
              </div>
            </div>

            {sel && (
              <div className="w-80 shrink-0 border-l border-border p-4">
                <div className="mb-3 flex items-center justify-between">
                  <span className="font-mono text-xs">{sel.method} {sel.path}</span>
                  <button onClick={() => setSel(null)} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
                </div>
                <Field label="Request ID" value={`${selRequestId}-${sel.ts_ms}`} mono />
                <Field label="Path" value={sel.path} mono />
                <Field label="Host" value={sel.host} mono />
                <Field label="Status" value={String(sel.status || "—")} />
                <div className="mt-3 flex items-center gap-1.5 text-xs text-secondary">
                  <MapPin className="h-3.5 w-3.5" /> Received in {regionLabel(sel.region)} ({sel.region})
                </div>
                <div className="mt-4 rounded-lg border border-border p-3">
                  <div className="mb-1.5 flex items-center gap-1.5 text-sm font-medium"><ShieldCheck className="h-4 w-4" /> Firewall</div>
                  <Badge tone={sel.action.includes("deny") || sel.action.includes("block") ? "red" : "green"}>
                    {sel.action.includes("deny") || sel.action.includes("block") ? "Blocked" : "Allowed"}
                  </Badge>
                </div>
                <div className="mt-3 rounded-lg border border-border p-3">
                  <div className="mb-2 flex items-center gap-1.5 text-sm font-medium"><Zap className="h-4 w-4" /> Function Invocation</div>
                  <Field label="Action" value={sel.action} />
                  {sel.detail ? <Field label="Detail" value={sel.detail} mono /> : null}
                  <Field label="Cache" value={(sel.cache || "miss").toUpperCase()} />
                  <Field label="Level" value={(sel.level || "info").toUpperCase()} />
                </div>
                <div className="mt-3 text-xs text-secondary">
                  <div className="mb-1 font-medium text-fg">Deployment Information</div>
                  <Field label="Project" value={project} />
                  <Field label="Environment" value={sel.env || "unknown"} />
                  <Field label="Resource" value={sel.resource || "unknown"} />
                  {sel.deployment ? <Field label="Deployment" value={sel.deployment} mono /> : null}
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function Level({ label, n, checked, onChange }: { label: string; n: number; checked: boolean; onChange: (on: boolean) => void }) {
  return (
    <label className="flex items-center justify-between py-1 text-xs text-secondary">
      <span className="flex items-center gap-2">
        <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} className="h-3.5 w-3.5 accent-fg" /> {label}
      </span>
      <span className="rounded-full bg-card px-1.5 text-[10px]">{n}</span>
    </label>
  );
}

function FilterRow({ label, value, options, onChange }: { label: string; value: string; options: string[]; onChange: (v: string) => void }) {
  return (
    <label className="flex items-center justify-between gap-2 py-1 hover:text-fg">
      <span className="shrink-0">{label}</span>
      <select
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="max-w-[110px] rounded-md border border-border bg-card px-1.5 py-1 text-xs text-secondary focus:outline-none"
      >
        <option value="">All</option>
        {options.map((o) => (
          <option key={o} value={o}>
            {o}
          </option>
        ))}
      </select>
    </label>
  );
}

function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="mb-2">
      <div className="text-[11px] uppercase tracking-wide text-muted">{label}</div>
      <div className={cn("truncate text-xs", mono && "font-mono")}>{value}</div>
    </div>
  );
}

/** Known edge regions; anything else falls through to its own code rather than
 * rendering a bare id. */
function regionLabel(r: string) {
  const map: Record<string, string> = {
    iad1: "Washington, D.C., USA",
    sfo1: "San Francisco, USA",
    fra1: "Frankfurt, Germany",
    lax1: "Los Angeles, USA",
    "los-angeles": "Los Angeles, USA",
    sj: "San Jose, USA",
    sj2: "San Jose, USA",
    hk: "Hong Kong",
    bkk: "Bangkok, Thailand",
    va: "Virginia, USA",
    va2: "Virginia, USA",
    va3: "Virginia, USA",
    sp: "São Paulo, Brazil",
    fr: "Frankfurt, Germany",
    phx: "Phoenix, USA",
    orlando: "Orlando, USA",
    sin: "Singapore",
  };
  return map[r] || map[r.toLowerCase()] || r;
}
