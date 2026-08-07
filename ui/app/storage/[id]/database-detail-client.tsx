"use client";

import { useEffect, useState, use } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { ArrowLeft, Copy, Eye, EyeOff, Trash2, Check, Info } from "lucide-react";
import { Card, Badge, Button, PageHeader } from "@/components/ui";
import { apiGet, apiSend, usePoll, type Database } from "@/lib/api";
import { timeAgo, copyText } from "@/lib/utils";
import { toast } from "@/components/toast";
import { KindBadge } from "../kind";
import {
  managedEndpoint, managedNoEndpointReason, projectFromRouteId, reachOfHost, endpointHost,
  POOLING_NOTE, REACH_NOTE, type Reach,
} from "../db-model";
import { SqliteDatabaseDetail } from "./sqlite-detail";

/**
 * One storage detail route for every database. The id decides which backend
 * answers — `sqlite_<project>` is a browser-replicated database (the
 * `browser_db` contract), anything else is a managed engine record from
 * `/v1/databases`. Both render the same shell.
 */
export function DatabaseDetail({ paramsPromise }: { paramsPromise: Promise<{ id: string }> }) {
  const params = use(paramsPromise);
  const project = projectFromRouteId(params.id);
  if (project) return <SqliteDatabaseDetail project={project} />;
  return <ManagedDatabaseDetail id={params.id} />;
}

function ManagedDatabaseDetail({ id }: { id: string }) {
  const router = useRouter();
  // ADAPTIVE: a ready database record is essentially static — poll fast (3s)
  // only while provisioning, 20s once ready (mutations invalidate the cache,
  // so a delete/redeploy still reflects immediately).
  const [provisioning, setProvisioning] = useState(true);
  const { data: db, error } = usePoll<Database>(`/v1/databases/${id}`, provisioning ? 3000 : 20000);
  useEffect(() => {
    if (db) setProvisioning(db.status === "provisioning");
  }, [db]);
  const [revealed, setRevealed] = useState<Record<string, string> | null>(null);
  const [copied, setCopied] = useState("");

  // The list/detail poll returns the connection with secrets MASKED (any key with
  // url/password/token/secret). Copying that yields "••••" junk. So both Reveal
  // AND Copy fetch the UNMASKED credentials from the tenant-scoped `/credentials`
  // endpoint; we cache them so a copy never depends on the user clicking Reveal.
  async function fetchCreds(): Promise<Record<string, string>> {
    if (revealed) return revealed;
    const full = await apiGet<Database>(`/v1/databases/${id}/credentials`, { fresh: true });
    const conn = full.connection ?? {};
    return conn;
  }
  async function reveal() {
    try {
      setRevealed(await fetchCreds());
    } catch (e) {
      toast(`Couldn't load credentials: ${String(e).replace(/^Error:\s*/, "")}`, {});
    }
  }
  // Copy the REAL value (fetches unmasked creds on demand), via a clipboard helper
  // that falls back to execCommand when the async API is unavailable — so the
  // button works everywhere, and reports success/failure instead of silently no-op.
  async function copy(k: string) {
    try {
      const conn = await fetchCreds();
      const value = conn[k];
      if (!value) {
        toast(`No value available for ${k}`, {});
        return;
      }
      const ok = await copyText(value);
      if (ok) {
        setCopied(k);
        setTimeout(() => setCopied(""), 1200);
        toast(`Copied ${k}`, { tone: "blue" });
      } else {
        toast("Copy failed — select the value and copy manually", {});
      }
    } catch (e) {
      toast(`Couldn't copy: ${String(e).replace(/^Error:\s*/, "")}`, {});
    }
  }
  async function destroy() {
    if (!confirm(`Delete ${db?.name}? This tears down the backing service.`)) return;
    await apiSend("DELETE", `/v1/databases/${id}`);
    router.push("/storage");
  }

  if (!db) {
    // Show the fetch error (e.g. slow/unreachable backend) rather than hanging on
    // "Loading…" forever, which under high latency reads as a broken page.
    if (error) {
      return (
        <div className="flex flex-col items-start gap-2 text-sm">
          <Link href="/storage" className="inline-flex items-center gap-1 text-secondary hover:text-fg"><ArrowLeft className="h-4 w-4" /> Storage</Link>
          <p className="text-red-500">Couldn&apos;t load this database: {String(error).replace(/^Error:\s*/, "")}</p>
        </div>
      );
    }
    return <div className="text-sm text-secondary">Loading…</div>;
  }
  // conn may be absent for a still-provisioning or replica record — never let
  // Object.entries throw (that produced a blank page).
  const conn = revealed ?? db.connection ?? {};
  const endpoint = managedEndpoint(db);

  return (
    <div>
      <Link href="/storage" className="mb-4 inline-flex items-center gap-1 text-sm text-secondary hover:text-fg"><ArrowLeft className="h-4 w-4" /> Storage</Link>
      <PageHeader
        title={db.name}
        desc={`${db.kind.toUpperCase()} · ${db.region} · ${db.project || "no project"}`}
        action={
          <div className="flex items-center gap-2">
            {db.status === "ready" ? <Badge tone="green">Ready{db.mode === "simulated" ? " · simulated" : " · live"}</Badge> : <Badge tone="amber">{db.status}</Badge>}
            <Button variant="danger" onClick={destroy}><Trash2 className="h-4 w-4" /> Delete</Button>
          </div>
        }
      />

      <div className="mb-6 grid grid-cols-2 gap-4 sm:grid-cols-4">
        <Mini label="Type" value={db.kind} />
        <Mini label="Region" value={db.region} />
        <Mini label="Mode" value={db.mode} />
        <Mini label="Created" value={db.created_ms ? `${timeAgo(db.created_ms)} ago` : "unknown"} />
      </div>

      {db.mode === "simulated" && (
        <Card className="mb-4 border-amber-500/40 bg-amber-500/5 text-sm">
          <div className="flex items-center gap-2 font-medium text-amber-500">This database is simulated</div>
          <p className="mt-1 text-secondary">
            No live backing engine was provisioned, so the connection details below don&apos;t point at a
            running instance yet. Delete and recreate it once the region has a container engine available.
          </p>
        </Card>
      )}

      {/* Primary connection string — the value most people want to copy, with an
          honest reachability verdict next to it: a loopback DSN is real but only
          resolves on the node running the engine. */}
      {(() => {
        const primaryKey = conn["DATABASE_URL"] ? "DATABASE_URL" : conn["REDIS_URL"] ? "REDIS_URL" : conn["endpoint"] ? "endpoint" : "";
        if (!primaryKey) {
          return (
            <Card className="mb-4">
              <div className="mb-2 flex items-center gap-2">
                <h3 className="text-sm font-semibold">Connection string</h3>
                <KindBadge kind={db.kind} />
              </div>
              <p className="text-sm text-secondary">{managedNoEndpointReason(db)}</p>
            </Card>
          );
        }
        const reach: Reach = endpoint ? endpoint.reach : "unknown";
        return (
          <Card className="mb-4">
            <div className="mb-2 flex items-center gap-2">
              <span className="text-xs font-medium uppercase tracking-wide text-muted">{primaryKey}</span>
              {reach === "internal" && <span title={REACH_NOTE.internal}><Badge tone="amber">node-local</Badge></span>}
              {reach === "external" && <span title={REACH_NOTE.external}><Badge tone="green">public</Badge></span>}
            </div>
            <div className="flex items-center gap-3">
              <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-subtle/50 px-3 py-2 font-mono text-xs">{conn[primaryKey]}</code>
              <Button variant="outline" onClick={() => copy(primaryKey)}>
                {copied === primaryKey ? <><Check className="h-4 w-4 text-green" /> Copied</> : <><Copy className="h-4 w-4" /> Copy</>}
              </Button>
            </div>
            {reach === "internal" && (
              <p className="mt-2 text-xs text-amber-600 dark:text-amber-400">{REACH_NOTE.internal}</p>
            )}
          </Card>
        );
      })()}

      <Card>
        <div className="mb-4 flex items-center justify-between">
          <h3 className="text-sm font-semibold">Connection</h3>
          <Button variant="outline" onClick={revealed ? () => setRevealed(null) : reveal}>
            {revealed ? <><EyeOff className="h-4 w-4" /> Hide</> : <><Eye className="h-4 w-4" /> Reveal secrets</>}
          </Button>
        </div>
        <div className="divide-y divide-border">
          {Object.entries(conn).length === 0 ? (
            <p className="py-2.5 text-xs text-muted">No connection details yet.</p>
          ) : (
            Object.entries(conn).map(([k, v]) => {
              // Classify only what we can actually see: a revealed URL by its own
              // host, and the platform's explicit `INTERNAL_*` naming convention.
              // A masked value is never guessed at.
              const rowReach: Reach | null = k.startsWith("INTERNAL_")
                ? "internal"
                : revealed && /:\/\//.test(v)
                ? reachOfHost(endpointHost(v))
                : null;
              return (
                <div key={k} className="flex items-center gap-3 py-2.5">
                  <div className="w-48 shrink-0 font-mono text-xs text-secondary">{k}</div>
                  <div className="min-w-0 flex-1 truncate font-mono text-xs text-fg">{v}</div>
                  {rowReach === "internal" && (
                    <span title={REACH_NOTE.internal} className="shrink-0"><Badge tone="amber">node-local</Badge></span>
                  )}
                  <button onClick={() => copy(k)} className="shrink-0 text-muted hover:text-fg" title={`Copy ${k}`}>
                    {copied === k ? <Check className="h-3.5 w-3.5 text-green" /> : <Copy className="h-3.5 w-3.5" />}
                  </button>
                </div>
              );
            })
          )}
        </div>
        {!revealed && (
          <p className="mt-3 text-xs text-muted">Reveal secrets to see the real values and which of them are externally reachable.</p>
        )}
        <p className="mt-3 flex items-start gap-1.5 text-xs text-muted">
          <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>{POOLING_NOTE}</span>
        </p>
        {db.container && <p className="mt-3 text-xs text-muted">Backed by container <code className="font-mono">{db.container}</code></p>}
      </Card>

      {db.note && <Card className="mt-4 text-sm text-secondary">{db.note}</Card>}
    </div>
  );
}

function Mini({ label, value }: { label: string; value: string }) {
  return (
    <Card className="flex flex-col gap-1">
      <span className="text-xs font-medium uppercase tracking-wide text-muted">{label}</span>
      <span className="truncate text-sm font-medium text-fg">{value}</span>
    </Card>
  );
}
