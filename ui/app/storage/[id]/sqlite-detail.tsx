"use client";

import { useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import {
  ArrowLeft, Copy, Check, Trash2, Globe2, Users, Info, Rocket, Pencil, FileStack,
} from "lucide-react";
import { Card, Badge, Button, PageHeader } from "@/components/ui";
import {
  apiGet, apiSend, usePoll,
  type Deployment, type ProjectSettings, type BrowserDbPolicy,
} from "@/lib/api";
import { timeAgo, copyText } from "@/lib/utils";
import { toast } from "@/components/toast";
import { RedeployModal } from "@/components/redeploy-modal";
import { KindBadge } from "../kind";
import { sqliteDatabases, sqliteConnectivity, SQLITE_NO_DSN, type SqliteDb } from "../db-model";
import { BrowserDbConfigModal, BrowserDbStatusPanel, formatBytes } from "../browser-db-panel";

/**
 * Detail page for a browser-replicated SQLite database — deliberately the same
 * shell as the managed-engine detail (back link, PageHeader + status + Delete,
 * a Mini stat row, a Connection card, then the specifics). The ONE honest
 * difference is inside the Connection card: this lane has no server-side wire
 * endpoint, so it states what is missing and what it costs instead of printing
 * a DSN that would connect to nothing.
 */
export function SqliteDatabaseDetail({ project }: { project: string }) {
  const router = useRouter();
  const { data: deployments } = usePoll<Deployment[]>("/deployments", 15000);
  const [settingsPolicy, setSettingsPolicy] = useState<BrowserDbPolicy | null | undefined>(undefined);
  // Bumped after a save so the dashboard-managed mirror is re-read fresh; the
  // load itself lives in the effect (never a setState in the effect BODY) and
  // drops its result when the project changes mid-flight.
  const [settingsNonce, setSettingsNonce] = useState(0);
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const s = await apiGet<ProjectSettings>(
          `/v1/projects/${encodeURIComponent(project)}/settings`,
          { fresh: settingsNonce > 0 },
        );
        if (!cancelled) setSettingsPolicy(s.browser_db ?? null);
      } catch {
        if (!cancelled) setSettingsPolicy(null);
      }
    })();
    return () => { cancelled = true; };
  }, [project, settingsNonce]);

  // BOTH sources must have answered before a verdict: deciding "not deployed"
  // from a half-loaded view would flash a wrong status on every page open.
  const loading = settingsPolicy === undefined || !deployments;
  const db: SqliteDb | null = useMemo(() => {
    if (loading) return null;
    const all = sqliteDatabases(deployments ?? [], { [project]: settingsPolicy });
    return all.find((d) => d.project === project) ?? null;
  }, [loading, deployments, settingsPolicy, project]);

  const projects = useMemo(
    () => Array.from(new Set((deployments ?? []).map((d) => d.project))).sort(),
    [deployments],
  );

  const [editing, setEditing] = useState(false);
  const [redeploying, setRedeploying] = useState(false);
  const [copied, setCopied] = useState("");

  async function copy(key: string, value: string) {
    if (await copyText(value)) {
      setCopied(key);
      setTimeout(() => setCopied(""), 1200);
      toast(`Copied ${key}`, { tone: "blue" });
    } else {
      toast("Copy failed — select the value and copy manually", {});
    }
  }

  async function removeConfig() {
    if (!confirm(`Remove the SQLite database config for "${project}"? Fleet replica data is kept (30-day inert grace) — this only stops new grants and a future deploy from carrying the block.`)) return;
    try {
      await apiSend("DELETE", `/v1/projects/${encodeURIComponent(project)}/browser-db`);
      router.push("/storage");
    } catch (e) {
      toast(`Couldn't remove: ${String(e).replace(/^Error:\s*/, "")}`, {});
    }
  }

  if (!db) {
    return (
      <div className="flex flex-col items-start gap-2 text-sm">
        <Link href="/storage" className="inline-flex items-center gap-1 text-secondary hover:text-fg"><ArrowLeft className="h-4 w-4" /> Storage</Link>
        {loading
          ? <p className="text-secondary">Loading…</p>
          : <p className="text-secondary">No SQLite database is configured for project <code className="font-mono">{project}</code>.</p>}
      </div>
    );
  }

  const { how, missing } = sqliteConnectivity();
  const fluidJson = JSON.stringify({ browser_db: db.policy }, null, 2);

  return (
    <div>
      <Link href="/storage" className="mb-4 inline-flex items-center gap-1 text-sm text-secondary hover:text-fg"><ArrowLeft className="h-4 w-4" /> Storage</Link>
      <PageHeader
        title={db.project}
        desc={`SQLITE · global · ${db.project}`}
        action={
          <div className="flex items-center gap-2">
            {db.live
              ? <Badge tone="green">Ready · replicated</Badge>
              : <Badge tone="amber">{db.configured ? "pending redeploy" : "not deployed"}</Badge>}
            <Button variant="outline" onClick={() => setEditing(true)}><Pencil className="h-4 w-4" /> Edit</Button>
            {db.redeployTarget && (
              <Button variant="outline" onClick={() => setRedeploying(true)}><Rocket className="h-4 w-4" /> Redeploy</Button>
            )}
            <Button variant="danger" onClick={removeConfig}><Trash2 className="h-4 w-4" /> Delete</Button>
          </div>
        }
      />

      <div className="mb-6 grid grid-cols-2 gap-4 sm:grid-cols-4">
        <Mini label="Type" value="browser_sqlite" />
        <Mini label="Scope" value="global" title="Replicated on every fleet node and in every admitted browser." />
        <Mini label="Access" value={db.policy.public_read ? "public read" : "team only"} />
        <Mini
          label="Live since"
          value={db.liveDeployment ? `${timeAgo(db.liveDeployment.created_at_ms)} ago` : "not deployed"}
          title="The deployment whose manifest carries this project's browser_db block."
        />
      </div>

      {!db.live && (
        <Card className="mb-4 border-amber-500/40 bg-amber-500/5 text-sm">
          <div className="flex items-center gap-2 font-medium text-amber-500">Not carried by a Ready deployment yet</div>
          <p className="mt-1 text-secondary">
            The config is saved, but the fleet acts on the deployment manifest. Redeploy this project so the block
            reaches a Ready deployment — until then no browser gets a grant and no replica is reconciled.
          </p>
        </Card>
      )}

      {/* The connection surface — stated, not faked. */}
      <Card className="mb-4">
        <div className="mb-2 flex items-center gap-2">
          <FileStack className="h-4 w-4 text-muted" />
          <h3 className="text-sm font-semibold">Connection</h3>
          <KindBadge kind="browser_sqlite" />
        </div>
        <p className="text-sm text-fg">{SQLITE_NO_DSN}</p>
        <div className="mt-3 grid gap-4 md:grid-cols-2">
          <div>
            <div className="mb-1.5 text-xs font-medium uppercase tracking-wide text-muted">How apps reach it</div>
            <ul className="flex flex-col gap-1.5 text-xs text-secondary">
              {how.map((line) => <li key={line} className="flex gap-1.5"><span className="text-muted">·</span><span>{line}</span></li>)}
            </ul>
          </div>
          <div>
            <div className="mb-1.5 text-xs font-medium uppercase tracking-wide text-muted">Not available</div>
            <ul className="flex flex-col gap-1.5 text-xs text-secondary">
              {missing.map((line) => <li key={line} className="flex gap-1.5"><span className="text-muted">·</span><span>{line}</span></li>)}
            </ul>
          </div>
        </div>
        <div className="mt-4 divide-y divide-border border-t border-border">
          <CopyRow
            label="replica file"
            value={db.replicaFile}
            hint="Platform-derived from the project name; no wire field ever names a file."
            copied={copied === "replica file"}
            onCopy={() => copy("replica file", db.replicaFile)}
          />
          <CopyRow
            label="fluid.json"
            value={fluidJson}
            multiline
            hint="The repo-authored opt-in. An explicit block in fluid.json always wins over this dashboard config."
            copied={copied === "fluid.json"}
            onCopy={() => copy("fluid.json", fluidJson)}
          />
        </div>
      </Card>

      <div className="mb-4 grid gap-4 lg:grid-cols-2">
        <Card>
          <h3 className="mb-3 text-sm font-semibold">Schema</h3>
          {db.policy.schema.length === 0 ? (
            <p className="text-xs text-muted">No tables defined.</p>
          ) : (
            <div className="flex flex-col gap-2">
              {db.policy.schema.map((t) => (
                <div key={t.name} className="rounded-lg border border-border p-3">
                  <div className="mb-1 flex items-center justify-between">
                    <code className="font-mono text-xs font-medium text-fg">{t.name}</code>
                    <button onClick={() => copy(t.name, t.ddl)} className="text-muted hover:text-fg" title={`Copy ${t.name} DDL`}>
                      {copied === t.name ? <Check className="h-3.5 w-3.5 text-green" /> : <Copy className="h-3.5 w-3.5" />}
                    </button>
                  </div>
                  <pre className="overflow-x-auto whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed text-secondary">{t.ddl || "— no columns —"}</pre>
                </div>
              ))}
            </div>
          )}
          <p className="mt-3 flex items-start gap-1.5 text-[11px] text-muted">
            <Info className="mt-0.5 h-3 w-3 shrink-0" />
            cr-sqlite does not replicate schema: both halves apply this DDL from the deployment spec, so a schema
            change needs a redeploy.
          </p>
        </Card>

        <Card>
          <h3 className="mb-3 text-sm font-semibold">Limits</h3>
          <div className="divide-y divide-border text-xs">
            <div className="flex items-center justify-between py-2.5">
              <span className="text-secondary">Max database size</span>
              <span className="font-mono text-fg">{db.policy.max_bytes ? formatBytes(db.policy.max_bytes) : "platform default"}</span>
            </div>
            <div className="flex items-center justify-between py-2.5">
              <span className="text-secondary">Max single value</span>
              <span className="font-mono text-fg">{db.policy.max_value_bytes ? formatBytes(db.policy.max_value_bytes) : "platform default"}</span>
            </div>
            <div className="flex items-center justify-between py-2.5">
              <span className="text-secondary">Anonymous access</span>
              <span>
                {db.policy.public_read
                  ? <Badge tone="blue"><Globe2 className="h-3 w-3" /> public read</Badge>
                  : <Badge tone="default"><Users className="h-3 w-3" /> team only</Badge>}
              </span>
            </div>
            <div className="flex items-center justify-between py-2.5">
              <span className="text-secondary">Spec source</span>
              <span className="text-fg">{db.live ? "deployment manifest" : db.configured ? "dashboard (not deployed)" : "unknown"}</span>
            </div>
          </div>
          <p className="mt-3 text-[11px] text-muted">
            Both caps bind the browser replica AND every fleet replica. An over-cap batch is refused and rolled
            back whole — rows are never truncated or evicted to fit.
          </p>
        </Card>
      </div>

      <Card>
        <h3 className="mb-3 text-sm font-semibold">Replication</h3>
        <BrowserDbStatusPanel project={db.project} />
      </Card>

      {editing && (
        <BrowserDbConfigModal
          projects={projects}
          initialProject={db.project}
          allowProjectPick={false}
          initial={db.policy}
          repoAuthored={db.live && !db.configured}
          onClose={() => setEditing(false)}
          onSaved={() => {
            setEditing(false);
            setSettingsNonce((n) => n + 1);
            if (db.redeployTarget) setRedeploying(true);
          }}
        />
      )}

      {redeploying && db.redeployTarget && (
        <RedeployModal
          deployment={db.redeployTarget}
          prodAlias={`${db.project}.localhost`}
          onClose={() => setRedeploying(false)}
        />
      )}
    </div>
  );
}

function Mini({ label, value, title }: { label: string; value: string; title?: string }) {
  return (
    <Card className="flex flex-col gap-1" title={title}>
      <span className="text-xs font-medium uppercase tracking-wide text-muted">{label}</span>
      <span className="truncate text-sm font-medium text-fg">{value}</span>
    </Card>
  );
}

function CopyRow({
  label, value, hint, multiline, copied, onCopy,
}: {
  label: string; value: string; hint: string; multiline?: boolean; copied: boolean; onCopy: () => void;
}) {
  return (
    <div className="flex items-start gap-3 py-2.5">
      <div className="w-32 shrink-0">
        <div className="font-mono text-xs text-secondary">{label}</div>
        <div className="mt-0.5 text-[11px] text-muted">{hint}</div>
      </div>
      <div className="min-w-0 flex-1">
        {multiline ? (
          <pre className="max-h-40 overflow-auto rounded-md border border-border bg-subtle/50 p-2 font-mono text-[11px] leading-relaxed text-fg">{value}</pre>
        ) : (
          <code className="block truncate font-mono text-xs text-fg">{value}</code>
        )}
      </div>
      <button onClick={onCopy} className="shrink-0 text-muted hover:text-fg" title={`Copy ${label}`}>
        {copied ? <Check className="h-3.5 w-3.5 text-green" /> : <Copy className="h-3.5 w-3.5" />}
      </button>
    </div>
  );
}
