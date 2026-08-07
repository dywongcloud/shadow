"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import {
  Database as DbIcon, Plus, X, Loader2, ChevronRight, ArrowLeft, Search, ExternalLink,
  Gauge, Copy, Check, FileStack, Info,
} from "lucide-react";
import { Card, Badge, Button, Input, PageHeader, Table, Th, Td } from "@/components/ui";
import { BrandIcon, BlobIcon, nativePng } from "@/components/provider-icons";
import {
  apiGet, apiSend, usePoll,
  type Database, type DbKind, type Deployment, type ProjectSettings, type BrowserDbPolicy,
} from "@/lib/api";
import { timeAgo, copyText } from "@/lib/utils";
import { toast } from "@/components/toast";
import Image from "next/image";
import { RedeployModal } from "@/components/redeploy-modal";
import { KIND_ICON, KindBadge } from "./kind";
import {
  KIND_LABEL, POOLING_NOTE, REACH_NOTE, sqliteDatabases, unifiedDatabases,
  type UnifiedDb, type UnifiedKind,
} from "./db-model";
import { BrowserDbConfigForm, emptyPolicy, saveBrowserDb } from "./browser-db-panel";

// Every primitive the platform provisions itself. SQLite sits in this list, not
// in a section of its own: it is a database, created the same way as the rest.
const NATIVE: { kind: UnifiedKind; name: string; desc: string }[] = [
  { kind: "postgres", name: "Postgres", desc: "Managed Postgres — instant, branchable." },
  { kind: "sqlite", name: "SQLite", desc: "Managed SQLite over libsql/Hrana — real libsql:// DSN, pooled." },
  { kind: "browser_sqlite", name: "SQLite (browser)", desc: "Replicated cr-sqlite — a live copy in every admitted browser." },
  { kind: "redis", name: "Redis", desc: "Durable key-value, TCP + REST." },
  { kind: "blob", name: "Blob", desc: "Fast S3-compatible object storage." },
  { kind: "queue", name: "Queue", desc: "Durable FIFO message queue." },
  { kind: "vector", name: "Vector", desc: "Embeddings index with cosine search." },
  { kind: "pubsub", name: "Pub/Sub", desc: "Topic-based fan-out (RabbitMQ-style)." },
  { kind: "realtime", name: "Realtime", desc: "Secure WebSocket streaming channels." },
];

// Marketplace providers → backed by a native kind. Each entry names the engine it
// actually provisions; a provider whose product the platform cannot really stand
// in for does not belong here (Turso was listed as "Serverless SQLite" while
// provisioning a Postgres container — the native SQLite entry above is the
// honest version of that offer).
interface Provider { id: string; name: string; desc: string; kind: DbKind; color: string }
const MARKETPLACE: Provider[] = [
  { id: "neon", name: "Neon", desc: "Serverless Postgres", kind: "postgres", color: "#00e599" },
  { id: "upstash", name: "Upstash", desc: "Serverless DB (Redis, Vector, Queue, Search)", kind: "redis", color: "#00c98d" },
  { id: "redis", name: "Redis", desc: "Official Redis", kind: "redis", color: "#d82c20" },
  { id: "aws", name: "AWS", desc: "Serverless, reliable, secure AWS services", kind: "blob", color: "#ff9900" },
  { id: "supabase", name: "Supabase", desc: "Postgres backend", kind: "postgres", color: "#3ecf8e" },
  { id: "nile", name: "Nile", desc: "Postgres re-engineered for B2B", kind: "postgres", color: "#5b8def" },
  { id: "motherduck", name: "MotherDuck", desc: "Analytics database", kind: "postgres", color: "#ff7a45" },
  { id: "convex", name: "Convex", desc: "Reactive database", kind: "postgres", color: "#f3336b" },
  { id: "prisma", name: "Prisma Postgres", desc: "Instant Serverless Postgres", kind: "postgres", color: "#5a67d8" },
  { id: "drizzle", name: "Drizzle", desc: "TypeScript ORM (Postgres)", kind: "postgres", color: "#c5f74f" },
  { id: "mongodb", name: "MongoDB Atlas", desc: "Database for developers", kind: "blob", color: "#13aa52" },
];

/**
 * Both halves of the tenant's database list, in one hook so the page never has
 * to know which backend a row came from.
 *
 * The managed list is one endpoint. The SQLite list is assembled from the two
 * replicated sources that actually decide whether a project HAS one: the
 * deployment manifests (gossiped `DeploymentInfo.browser_db`, what the fleet
 * reconciles against) and the dashboard-managed `ProjectSettings.browser_db`
 * mirror. See `sqliteDatabases`.
 */
function useDatabases() {
  const [provisioning, setProvisioning] = useState(false);
  const { data: managed, error, refresh } = usePoll<Database[]>("/v1/databases", provisioning ? 3000 : 12000);
  useEffect(() => {
    setProvisioning((managed ?? []).some((d) => d.status === "provisioning"));
  }, [managed]);

  const { data: deployments } = usePoll<Deployment[]>("/deployments", 15000);
  const projects = useMemo(
    () => Array.from(new Set((deployments ?? []).map((d) => d.project))).sort(),
    [deployments],
  );
  const projectsKey = projects.join(",");

  const [settings, setSettings] = useState<Record<string, BrowserDbPolicy | null | undefined>>({});
  const refreshProject = useCallback(async (project: string) => {
    try {
      const s = await apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`, { fresh: true });
      setSettings((cur) => ({ ...cur, [project]: s.browser_db ?? null }));
    } catch {
      /* leave the stale entry — the next load retries */
    }
  }, []);

  // A SQLite database must not blink out of the list because one of N parallel
  // per-project fetches failed. Two rules make the lane as stable as the managed
  // half, which is one endpoint and cannot partially fail:
  //
  //  * MERGE, never replace. `undefined` means "this fetch failed", and a failed
  //    fetch must leave the last known answer standing. Only a definite result
  //    (a policy, or `null` for "this project has no browser_db") overwrites.
  //  * RETRY the unknowns. The effect keys on the project SET, so without this a
  //    project that failed once stayed unknown until the set itself changed —
  //    i.e. a transient blip removed a live database from the page permanently.
  useEffect(() => {
    if (!projects.length) return;
    let cancelled = false;

    const load = async (targets: string[]) => {
      if (!targets.length) return;
      const entries = await Promise.all(
        targets.map(async (p) => {
          try {
            const s = await apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(p)}/settings`);
            return [p, s.browser_db ?? null] as const;
          } catch {
            return [p, undefined] as const;
          }
        }),
      );
      if (cancelled) return;
      setSettings((cur) => {
        const next = { ...cur };
        for (const [p, v] of entries) if (v !== undefined) next[p] = v;
        return next;
      });
    };

    void load(projects);
    const retry = setInterval(() => {
      setSettings((cur) => {
        const unknown = projects.filter((p) => cur[p] === undefined);
        if (unknown.length) void load(unknown);
        return cur;
      });
    }, 20000);

    return () => { cancelled = true; clearInterval(retry); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectsKey]);

  const sqlite = useMemo(() => sqliteDatabases(deployments ?? [], settings), [deployments, settings]);
  const rows = useMemo(() => unifiedDatabases(managed ?? [], sqlite), [managed, sqlite]);

  // `managed` null AND no sqlite row = still loading / genuinely nothing. The
  // page distinguishes those with `error`.
  return { rows, error, refresh, refreshProject, projects, deployments: deployments ?? [], loaded: managed !== null };
}

export default function StoragePage() {
  const { rows, error, refresh, refreshProject, projects, deployments, loaded } = useDatabases();
  const [open, setOpen] = useState(false);
  const [redeployTarget, setRedeployTarget] = useState<Deployment | null>(null);

  // Deep-link: /storage?browse opens the Browse Storage panel directly.
  useEffect(() => {
    if (typeof window !== "undefined" && new URLSearchParams(window.location.search).has("browse")) {
      setOpen(true);
    }
  }, []);

  return (
    <div className="pb-24">
      <PageHeader
        title="Storage"
        desc="Every database in one place — managed engines and browser-replicated SQLite, provisioned per project."
        action={<Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Create Database</Button>}
      />

      {error && !rows.length ? (
        <Card className="flex flex-col items-center gap-3 py-16 text-center">
          <DbIcon className="h-8 w-8 text-muted" />
          <div className="text-sm font-medium">Couldn&apos;t load databases</div>
          <p className="max-w-sm text-sm text-secondary">{String(error).replace(/^Error:\s*/, "")}</p>
          <Button variant="outline" onClick={refresh}>Retry</Button>
        </Card>
      ) : !rows.length ? (
        <Card className="flex flex-col items-center gap-3 py-16 text-center">
          <DbIcon className="h-8 w-8 text-muted" />
          <div className="text-sm font-medium">{loaded ? "No databases yet" : "Loading databases…"}</div>
          <p className="max-w-sm text-sm text-secondary">
            Provision a serverless database and connect it to a project. Credentials are generated automatically —
            or deploy a replicated SQLite database that lives in your users&apos; browsers.
          </p>
          <Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Create Database</Button>
        </Card>
      ) : (
        <>
          <Table>
            <thead>
              <tr>
                <Th>Name</Th><Th>Provider</Th><Th>Type</Th><Th>Project</Th><Th>Region</Th>
                <Th>Connection</Th><Th>Status</Th><Th>Created</Th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.id} className="cursor-pointer hover:bg-subtle">
                  <Td className="font-medium">
                    <Link href={row.href} className="flex items-center gap-2">
                      <span className="text-muted">{KIND_ICON[row.kind]}</span>{row.name}
                    </Link>
                  </Td>
                  <Td className="text-secondary">{row.provider}</Td>
                  <Td><KindBadge kind={row.kind} /></Td>
                  <Td className="text-secondary">{row.project || "—"}</Td>
                  <Td>
                    <div className="flex flex-wrap items-center gap-1">
                      <span title={row.regionTitle}><Badge tone="blue">{row.region}</Badge></span>
                      {row.replicas.map((r) => (
                        <span key={r} title="read replica"><Badge tone="default">↳ {r}</Badge></span>
                      ))}
                    </div>
                  </Td>
                  <Td><ConnectionCell row={row} /></Td>
                  <Td><StatusBadge row={row} /></Td>
                  <Td className="text-secondary">
                    <span title={row.createdTitle}>{row.createdMs ? timeAgo(row.createdMs) : "—"}</span>
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
          <p className="mt-3 flex items-start gap-1.5 text-xs text-muted">
            <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{POOLING_NOTE}</span>
          </p>
        </>
      )}

      {open && (
        <BrowseStorage
          projects={projects}
          onClose={() => setOpen(false)}
          onCreated={() => { setOpen(false); refresh(); }}
          onSqliteSaved={async (project) => {
            setOpen(false);
            await refreshProject(project);
            const target = deployments
              .filter((d) => d.project === project)
              .sort((a, b) => b.created_at_ms - a.created_at_ms)[0];
            if (target) setRedeployTarget(target);
          }}
        />
      )}

      {redeployTarget && (
        <RedeployModal
          deployment={redeployTarget}
          prodAlias={`${redeployTarget.project}.localhost`}
          onClose={() => setRedeployTarget(null)}
        />
      )}
    </div>
  );
}

function StatusBadge({ row }: { row: UnifiedDb }) {
  if (row.status === "provisioning") {
    return (
      <Badge tone="amber">
        {row.source === "managed" ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
        {row.statusNote || "Provisioning"}
      </Badge>
    );
  }
  if (row.status === "error") return <Badge tone="red">Error</Badge>;
  return <Badge tone="green">Ready{row.statusNote ? ` · ${row.statusNote}` : ""}</Badge>;
}

/**
 * The connection cell — the one place the SQLite lane genuinely differs, and it
 * says so rather than showing a DSN that connects to nothing.
 *
 * For a managed engine the ADDRESS shown comes from the unmasked `host`/`port`
 * keys; the copy button fetches the real credential from
 * `/v1/databases/:id/credentials` (the list poll masks anything url/token/secret,
 * so copying the polled value would yield "••••" junk).
 */
function ConnectionCell({ row }: { row: UnifiedDb }) {
  const [copied, setCopied] = useState(false);

  if (!row.endpoint) {
    return (
      <Link href={row.href} className="text-xs text-muted hover:text-fg" title={row.noEndpointReason}>
        {row.source === "sqlite" ? "no wire endpoint" : "—"}
      </Link>
    );
  }

  const endpoint = row.endpoint;
  async function copy(e: React.MouseEvent) {
    e.preventDefault();
    e.stopPropagation();
    try {
      const full = await apiGet<Database>(`/v1/databases/${row.id}/credentials`, { fresh: true });
      const value = full.connection?.[endpoint.key] ?? "";
      if (!value) {
        toast(`No ${endpoint.key} available yet`, {});
        return;
      }
      if (await copyText(value)) {
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
        toast(`Copied ${endpoint.key}`, { tone: "blue" });
      } else {
        toast("Copy failed — open the database and copy manually", {});
      }
    } catch (err) {
      toast(`Couldn't copy: ${String(err).replace(/^Error:\s*/, "")}`, {});
    }
  }

  return (
    <span className="flex items-center gap-1.5">
      <code className="truncate font-mono text-xs text-secondary" title={`${endpoint.key} · ${endpoint.protocol}`}>
        {endpoint.address}
      </code>
      {endpoint.reach === "internal" && (
        <span title={REACH_NOTE.internal}><Badge tone="amber">internal</Badge></span>
      )}
      <button onClick={copy} className="shrink-0 text-muted hover:text-fg" title={`Copy ${endpoint.key}`}>
        {copied ? <Check className="h-3.5 w-3.5 text-green" /> : <Copy className="h-3.5 w-3.5" />}
      </button>
    </span>
  );
}

/** Vercel-style "Browse Storage" right-side slide-over — one create flow for
 *  every kind, SQLite included. */
function BrowseStorage({
  projects,
  onClose,
  onCreated,
  onSqliteSaved,
}: {
  projects: string[];
  onClose: () => void;
  onCreated: () => void;
  onSqliteSaved: (project: string) => void;
}) {
  const [q, setQ] = useState("");
  const [sel, setSel] = useState<{ kind: UnifiedKind; provider: string } | null>(null);
  const [step, setStep] = useState<"browse" | "configure">("browse");
  // managed-engine configure step
  const [name, setName] = useState("");
  const [project, setProject] = useState("");
  const [region, setRegion] = useState("san-jose");
  const [replicas, setReplicas] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  // sqlite configure step
  const [sqliteProject, setSqliteProject] = useState("");
  const [policy, setPolicy] = useState<BrowserDbPolicy>(emptyPolicy());
  // The browser-replicated lane is configured by a deployment block, not by
  // POST /v1/databases — the managed `sqlite` kind takes the ordinary path.
  const isSqlite = sel?.kind === "browser_sqlite";

  // Live mesh regions for the replica selector (falls back to the known set).
  const { data: catalog } = usePoll<Record<string, { id: string; label: string }[]>>("/v1/regions/catalog", 30000);
  const allRegions: string[] = catalog
    ? Array.from(new Set(Object.values(catalog).flat().map((r) => r.id)))
    : ["los-angeles", "san-jose", "virginia", "bangkok"];
  const toggleReplica = (r: string) =>
    setReplicas((cur) => (cur.includes(r) ? cur.filter((x) => x !== r) : [...cur, r]));

  const ql = q.toLowerCase();
  const native = NATIVE.filter((n) => !ql || n.name.toLowerCase().includes(ql) || n.desc.toLowerCase().includes(ql));
  const market = MARKETPLACE.filter((m) => !ql || m.name.toLowerCase().includes(ql) || m.desc.toLowerCase().includes(ql));

  async function create() {
    if (!sel) return;
    setBusy(true);
    setError("");
    if (isSqlite) {
      const err = await saveBrowserDb(sqliteProject || projects[0] || "", policy);
      if (err) {
        setError(err.replace(/^Error:\s*/, ""));
        setBusy(false);
        return;
      }
      onSqliteSaved(sqliteProject || projects[0] || "");
      return;
    }
    try {
      await apiSend("POST", "/v1/databases", {
        name: name || `${sel.provider.toLowerCase().replace(/[^a-z0-9]+/g, "-")}-db`,
        project: project || "default",
        kind: sel.kind,
        region,
        provider: sel.provider,
        replicas: replicas.filter((r) => r !== region),
      });
      onCreated();
    } catch (e) {
      setError(String(e).replace(/^Error:\s*/, ""));
      setBusy(false);
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex justify-end bg-black/40" onClick={onClose}>
      <div className="flex h-full w-full max-w-xl flex-col border-l border-border bg-card shadow-pop" onClick={(e) => e.stopPropagation()}>
        {/* header */}
        <div className="flex items-start justify-between border-b border-border p-6">
          <div>
            <h2 className="text-xl font-semibold">{step === "browse" ? "Browse Storage" : "Configure"}</h2>
            <p className="mt-1 text-sm text-secondary">
              {step === "browse"
                ? "Create databases and stores that you can connect to your projects."
                : isSqlite
                ? "SQLite (browser) · replicated to every admitted browser"
                : `${sel?.provider} · backed by OpenEdge ${sel ? KIND_LABEL[sel.kind] : ""}`}
            </p>
          </div>
          <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-5 w-5" /></button>
        </div>

        {/* body */}
        <div className="flex-1 overflow-y-auto p-6">
          {step === "browse" ? (
            <>
              <div className="relative mb-5">
                <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
                <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search provider or database…"
                  className="w-full rounded-lg border border-border bg-bg py-2.5 pl-9 pr-3 text-sm focus:outline-none focus:ring-2 focus:ring-border" />
              </div>

              {!!native.length && (
                <>
                  <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">OpenEdge Native</div>
                  <div className="mb-6 flex flex-col gap-2">
                    {native.map((n) => (
                      <Row key={n.kind}
                        icon={nativePng(n.kind)
                          ? <span className="flex h-9 w-9 items-center justify-center overflow-hidden rounded-lg bg-white/5"><Image src={nativePng(n.kind)!} alt={n.name} width={28} height={28} className="h-7 w-7 object-contain" /></span>
                          : n.kind === "blob"
                          ? <span className="flex h-9 w-9 items-center justify-center"><BlobIcon /></span>
                          : <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary">{KIND_ICON[n.kind]}</span>}
                        name={n.name} desc={n.desc}
                        active={sel?.provider === n.name}
                        onClick={() => setSel({ kind: n.kind, provider: n.name })}
                      />
                    ))}
                  </div>
                </>
              )}

              {!!market.length && (
                <>
                  <div className="mb-2 flex items-center justify-between">
                    <span className="text-xs font-medium uppercase tracking-wide text-muted">Marketplace Database Providers</span>
                    <span className="flex items-center gap-1 text-xs text-link">Learn more <ExternalLink className="h-3 w-3" /></span>
                  </div>
                  <div className="flex flex-col gap-2">
                    {market.map((m) => (
                      <Row key={m.id}
                        icon={<BrandIcon id={m.id} name={m.name} color={m.color} />}
                        name={m.name} desc={m.desc}
                        active={sel?.provider === m.name}
                        onClick={() => setSel({ kind: m.kind, provider: m.name })}
                      />
                    ))}
                  </div>
                </>
              )}
            </>
          ) : isSqlite ? (
            <div className="space-y-4">
              <div className="flex items-center gap-3 rounded-lg border border-border bg-subtle p-3">
                <span className="text-secondary"><FileStack className="h-5 w-5" /></span>
                <div>
                  <div className="text-sm font-medium">SQLite</div>
                  <div className="text-xs text-secondary">
                    cr-sqlite, replicated bidirectionally between the fleet and every admitted browser.
                  </div>
                </div>
              </div>
              <BrowserDbConfigForm
                projects={projects}
                project={sqliteProject || projects[0] || ""}
                onProjectChange={setSqliteProject}
                allowProjectPick
                policy={policy}
                onPolicyChange={setPolicy}
              />
              <p className="flex items-start gap-1.5 text-xs text-muted">
                <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                No connection string is issued: apps read and write this database through the replica in the
                browser. There is no libsql/Turso wire endpoint and no server-side pool for it.
              </p>
              {error && <p className="text-sm text-red-500">{error}</p>}
            </div>
          ) : (
            <div className="space-y-4">
              <div className="flex items-center gap-3 rounded-lg border border-border bg-subtle p-3">
                <span className="text-secondary">{sel && KIND_ICON[sel.kind]}</span>
                <div>
                  <div className="text-sm font-medium">{sel?.provider}</div>
                  <div className="text-xs text-secondary">Backed by OpenEdge {sel ? KIND_LABEL[sel.kind] : ""}</div>
                </div>
              </div>
              <div>
                <label className="mb-1 block text-xs font-medium text-secondary">Database name</label>
                <Input value={name} onChange={(e) => setName(e.target.value)} placeholder={`${sel?.provider?.toLowerCase()}-db`} />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div>
                  <label className="mb-1 block text-xs font-medium text-secondary">Project</label>
                  <Input value={project} onChange={(e) => setProject(e.target.value)} placeholder="my-project" />
                </div>
                <div>
                  <label className="mb-1 block text-xs font-medium text-secondary">Primary region</label>
                  <select
                    value={region}
                    onChange={(e) => setRegion(e.target.value)}
                    className="w-full rounded-md border border-border bg-bg px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-border"
                  >
                    {allRegions.map((r) => (
                      <option key={r} value={r}>{r}</option>
                    ))}
                  </select>
                </div>
              </div>
              <div>
                <label className="mb-1 block text-xs font-medium text-secondary">Replica regions <span className="text-muted">(cross-region reads)</span></label>
                <div className="flex flex-wrap gap-2">
                  {allRegions.filter((r) => r !== region).map((r) => (
                    <button
                      key={r}
                      type="button"
                      onClick={() => toggleReplica(r)}
                      className={
                        "rounded-md border px-2.5 py-1 text-xs transition-colors " +
                        (replicas.includes(r)
                          ? "border-fg bg-fg text-bg"
                          : "border-border text-secondary hover:bg-subtle")
                      }
                    >
                      {r}{replicas.includes(r) ? " ✓" : ""}
                    </button>
                  ))}
                </div>
                {replicas.length > 0 ? (
                  <p className="mt-1.5 text-xs text-muted">
                    Replicated to {replicas.filter((r) => r !== region).join(", ")} — reads served locally in each region.
                  </p>
                ) : null}
              </div>
              <p className="flex items-center gap-1.5 text-xs text-muted"><Gauge className="h-3.5 w-3.5" /> Credentials are generated automatically and injected into the project&apos;s environment variables.</p>
              {error && <p className="text-sm text-red-500">{error}</p>}
            </div>
          )}
        </div>

        {/* footer */}
        <div className="flex items-center justify-between border-t border-border p-4">
          {step === "configure"
            ? <Button variant="ghost" onClick={() => setStep("browse")}><ArrowLeft className="h-4 w-4" /> Back</Button>
            : <span className="text-xs text-muted">{sel ? `Selected: ${sel.provider}` : "Select a database or provider"}</span>}
          <div className="flex gap-2">
            <Button variant="outline" onClick={onClose}>Cancel</Button>
            {step === "browse" ? (
              <Button onClick={() => setStep("configure")} disabled={!sel}>Continue <ChevronRight className="h-4 w-4" /></Button>
            ) : (
              <Button onClick={create} disabled={busy}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />} Create</Button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function Row({ icon, name, desc, active, onClick }: { icon: React.ReactNode; name: string; desc: string; active: boolean; onClick: () => void }) {
  return (
    <button onClick={onClick} className={`flex w-full items-center gap-3 rounded-xl border px-4 py-3 text-left transition-colors ${active ? "border-fg bg-subtle" : "border-border hover:bg-subtle"}`}>
      {icon}
      <div className="min-w-0 flex-1">
        <div className="text-sm font-semibold">{name}</div>
        <div className="truncate text-xs text-secondary">{desc}</div>
      </div>
      <ChevronRight className="h-4 w-4 shrink-0 text-muted" />
    </button>
  );
}
