"use client";

import { useEffect, useRef, useState, use } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { ArrowLeft, Copy, Loader2, Play, Plus, Square, Trash2, X } from "lucide-react";
import { Badge, Button, Card, Input, PageHeader, Switch } from "@/components/ui";
import {
  apiGet,
  apiSend,
  usePoll,
  type SandboxRecord,
  type SandboxCommandRecord,
  type SandboxSnapshotRecord,
  type SandboxMountRecord,
  type NetworkPolicy,
} from "@/lib/api";
import { timeAgo, copyText } from "@/lib/utils";
import { toast } from "@/components/toast";

function statusTone(s: string): "green" | "amber" | "red" | "default" {
  if (s === "running") return "green";
  if (s === "pending" || s === "stopping") return "amber";
  if (s === "failed") return "red";
  return "default";
}

export function SandboxDetail({ paramsPromise }: { paramsPromise: Promise<{ project: string; sandboxId: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const sandboxId = params.sandboxId;
  const router = useRouter();
  const base = `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandboxId}`;
  const { data: sandbox, error, refresh } = usePoll<SandboxRecord>(base, 3000);

  async function stop() {
    await apiSend("POST", `${base}/stop`);
    refresh();
  }
  async function destroy() {
    if (!confirm(`Delete sandbox "${sandbox?.name}"? This cannot be undone.`)) return;
    await apiSend("DELETE", base);
    router.push(`/projects/${encodeURIComponent(project)}/sandboxes`);
  }

  if (!sandbox) {
    if (error) {
      return (
        <div className="flex flex-col items-start gap-2 text-sm">
          <Link href={`/projects/${encodeURIComponent(project)}/sandboxes`} className="inline-flex items-center gap-1 text-secondary hover:text-fg"><ArrowLeft className="h-4 w-4" /> Sandboxes</Link>
          <p className="text-red-500">Couldn&apos;t load this sandbox: {String(error).replace(/^Error:\s*/, "")}</p>
        </div>
      );
    }
    return <div className="text-sm text-secondary">Loading…</div>;
  }

  return (
    <div className="pb-24">
      <Link href={`/projects/${encodeURIComponent(project)}/sandboxes`} className="mb-4 inline-flex items-center gap-1 text-sm text-secondary hover:text-fg"><ArrowLeft className="h-4 w-4" /> Sandboxes</Link>
      <PageHeader
        title={sandbox.name}
        desc={`${sandbox.runtime} · ${sandbox.region} · ${sandbox.vcpus} vCPU · ${sandbox.memory_mb} MB`}
        action={
          <div className="flex items-center gap-2">
            <Badge tone={statusTone(sandbox.status)}>{sandbox.status}</Badge>
            {sandbox.status === "running" && <Button variant="outline" onClick={stop}><Square className="h-4 w-4" /> Stop</Button>}
            <Button variant="danger" onClick={destroy}><Trash2 className="h-4 w-4" /> Delete</Button>
          </div>
        }
      />

      {sandbox.note ? (
        <Card className="mb-4 border-amber-500/40 bg-amber-500/5 text-sm">
          <div className="font-medium text-amber-500">{sandbox.status === "failed" ? "This sandbox is simulated" : "Note"}</div>
          <p className="mt-1 text-secondary">{sandbox.note}</p>
        </Card>
      ) : null}

      <div className="mb-6 grid grid-cols-2 gap-4 lg:grid-cols-4">
        <StatusCard sandbox={sandbox} />
        <ResourcesCard sandbox={sandbox} />
        <FilesystemCard project={project} sandboxId={sandboxId} />
        <UsageCard sandbox={sandbox} />
      </div>

      <div className="flex flex-col gap-6">
        <RunCommandPanel project={project} sandboxId={sandboxId} />
        <FilesPanel project={project} sandboxId={sandboxId} />
        <PortsCard project={project} sandbox={sandbox} />
        <SnapshotsPanel project={project} sandboxId={sandboxId} onChanged={refresh} />
        <MountsPanel project={project} sandboxId={sandboxId} />
        <NetworkPolicyPanel project={project} sandboxId={sandboxId} sandbox={sandbox} onChanged={refresh} />
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Summary cards
// ---------------------------------------------------------------------------
function Mini({ label, value }: { label: string; value: string }) {
  return (
    <Card className="flex flex-col gap-1">
      <span className="text-xs font-medium uppercase tracking-wide text-muted">{label}</span>
      <span className="truncate text-sm font-medium text-fg">{value}</span>
    </Card>
  );
}

function StatusCard({ sandbox }: { sandbox: SandboxRecord }) {
  const remaining = sandbox.timeout_expires_at ? Math.max(0, sandbox.timeout_expires_at - Date.now()) : 0;
  const mins = Math.floor(remaining / 60000);
  return <Mini label="Timeout remaining" value={sandbox.status === "running" ? `${mins}m` : "—"} />;
}
function ResourcesCard({ sandbox }: { sandbox: SandboxRecord }) {
  return <Mini label="Resources" value={`${sandbox.vcpus} vCPU / ${sandbox.memory_mb} MB`} />;
}
function FilesystemCard({ project, sandboxId }: { project: string; sandboxId: string }) {
  return <Mini label="Filesystem" value="/build (default workspace)" />;
}
function UsageCard({ sandbox }: { sandbox: SandboxRecord }) {
  return <Mini label="Total duration" value={`${Math.round(sandbox.total_duration_ms / 1000)}s`} />;
}

// ---------------------------------------------------------------------------
// Run Command panel
// ---------------------------------------------------------------------------
function RunCommandPanel({ project, sandboxId }: { project: string; sandboxId: string }) {
  const [cmd, setCmd] = useState("");
  const [args, setArgs] = useState("");
  const [cwd, setCwd] = useState("");
  const [envText, setEnvText] = useState("");
  const [sudo, setSudo] = useState(false);
  const [detached, setDetached] = useState(false);
  const [running, setRunning] = useState(false);
  const [activeCommandId, setActiveCommandId] = useState<string | null>(null);
  const [record, setRecord] = useState<SandboxCommandRecord | null>(null);
  const [copied, setCopied] = useState(false);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const base = `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandboxId}`;

  function stopPolling() {
    if (pollRef.current) {
      clearInterval(pollRef.current);
      pollRef.current = null;
    }
  }
  useEffect(() => () => stopPolling(), []);

  async function pollLogs(commandId: string) {
    try {
      const r = await apiGet<SandboxCommandRecord>(`${base}/commands/${commandId}/logs`, { fresh: true });
      setRecord(r);
      if (r.status !== "running" && r.status !== "queued") {
        stopPolling();
        setRunning(false);
      }
    } catch {
      /* keep last known state */
    }
  }

  async function run() {
    setRunning(true);
    setRecord(null);
    try {
      const env: Record<string, string> = {};
      for (const line of envText.split("\n")) {
        const [k, ...rest] = line.split("=");
        if (k && k.trim()) env[k.trim()] = rest.join("=").trim();
      }
      const res = await apiSend<SandboxCommandRecord>("POST", `${base}/commands`, {
        cmd,
        args: args.split(" ").filter(Boolean),
        cwd,
        env,
        sudo,
        detached,
      });
      setActiveCommandId(res.id);
      setRecord(res);
      if (res.status === "running") {
        pollRef.current = setInterval(() => pollLogs(res.id), 1000);
      } else {
        setRunning(false);
      }
    } catch (e) {
      toast(String(e).replace(/^Error:\s*/, ""), {});
      setRunning(false);
    }
  }

  async function kill() {
    if (!activeCommandId) return;
    await apiSend("POST", `${base}/commands/${activeCommandId}/kill`);
    stopPolling();
    setRunning(false);
  }

  async function copyOutput() {
    if (!record) return;
    const text = [...record.stdout, ...record.stderr].sort((a, b) => a.ts_ms - b.ts_ms).map((l) => l.line).join("\n");
    const ok = await copyText(text);
    if (ok) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    }
  }

  return (
    <Card>
      <h3 className="mb-3 text-sm font-semibold">Run Command</h3>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs text-secondary">Command</label>
          <Input value={cmd} placeholder="node" className="font-mono" onChange={(e) => setCmd(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Args (space-separated)</label>
          <Input value={args} placeholder="--version" className="font-mono" onChange={(e) => setArgs(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Working directory</label>
          <Input value={cwd} placeholder="/build" className="font-mono" onChange={(e) => setCwd(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Environment variables (KEY=value per line)</label>
          <Input value={envText} placeholder="NODE_ENV=production" className="font-mono" onChange={(e) => setEnvText(e.target.value)} />
        </div>
      </div>
      <div className="mt-3 flex items-center gap-4">
        <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={sudo} onChange={(e) => setSudo(e.target.checked)} /> Sudo</label>
        <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={detached} onChange={(e) => setDetached(e.target.checked)} /> Detached</label>
        <div className="ml-auto flex gap-2">
          {running && activeCommandId ? <Button variant="danger" onClick={kill}><Square className="h-4 w-4" /> Kill</Button> : null}
          <Button onClick={run} disabled={running || !cmd.trim()}>
            {running ? <Loader2 className="h-4 w-4 animate-spin" /> : <Play className="h-4 w-4" />} Run
          </Button>
        </div>
      </div>

      {record && (
        <div className="mt-4 rounded-lg border border-border bg-subtle/40 p-3">
          <div className="mb-2 flex items-center justify-between">
            <div className="flex items-center gap-2 text-xs">
              <Badge tone={record.status === "exited" && record.exit_code === 0 ? "green" : record.status === "running" || record.status === "queued" ? "amber" : "red"}>{record.status}</Badge>
              {record.exit_code !== null && record.exit_code !== undefined ? <span className="text-secondary">exit code {record.exit_code}</span> : null}
            </div>
            <Button variant="ghost" onClick={copyOutput}>{copied ? "Copied" : <><Copy className="h-3.5 w-3.5" /> Copy output</>}</Button>
          </div>
          <pre className="max-h-72 overflow-auto whitespace-pre-wrap font-mono text-xs">
            {[...record.stdout.map((l) => ({ ...l, err: false })), ...record.stderr.map((l) => ({ ...l, err: true }))]
              .sort((a, b) => a.ts_ms - b.ts_ms)
              .map((l, i) => (
                <div key={i} className={l.err ? "text-red-500" : ""}>{l.line}</div>
              ))}
            {record.stdout.length === 0 && record.stderr.length === 0 ? <span className="text-muted">(no output yet)</span> : null}
          </pre>
        </div>
      )}
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Files panel
// ---------------------------------------------------------------------------
function FilesPanel({ project, sandboxId }: { project: string; sandboxId: string }) {
  const [path, setPath] = useState("/build/hello.txt");
  const [content, setContent] = useState("");
  const [readPath, setReadPath] = useState("/build/hello.txt");
  const [readResult, setReadResult] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const base = `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandboxId}`;

  async function write() {
    setBusy(true);
    try {
      const b64 = btoa(unescape(encodeURIComponent(content)));
      await apiSend("POST", `${base}/files/write`, { files: [{ path, content_b64: b64 }] });
      toast(`Wrote ${path}`, { tone: "blue" });
    } catch (e) {
      toast(String(e).replace(/^Error:\s*/, ""), {});
    } finally {
      setBusy(false);
    }
  }
  async function read() {
    setBusy(true);
    setReadResult(null);
    try {
      const r = await apiGet<{ path: string; content_b64: string }>(`${base}/files/read?path=${encodeURIComponent(readPath)}`, { fresh: true });
      setReadResult(decodeURIComponent(escape(atob(r.content_b64))));
    } catch (e) {
      toast(String(e).replace(/^Error:\s*/, ""), {});
    } finally {
      setBusy(false);
    }
  }
  async function mkdir() {
    setBusy(true);
    try {
      await apiSend("POST", `${base}/commands`, { cmd: "mkdir", args: ["-p", path.replace(/\/[^/]*$/, "")] });
      toast("Directory created", { tone: "blue" });
    } catch (e) {
      toast(String(e).replace(/^Error:\s*/, ""), {});
    } finally {
      setBusy(false);
    }
  }
  async function del() {
    setBusy(true);
    try {
      await apiSend("POST", `${base}/commands`, { cmd: "rm", args: ["-rf", readPath] });
      toast(`Deleted ${readPath}`, { tone: "blue" });
    } catch (e) {
      toast(String(e).replace(/^Error:\s*/, ""), {});
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <h3 className="mb-1 text-sm font-semibold">Files</h3>
      <p className="mb-3 text-xs text-secondary">Current working directory: <code className="font-mono">/build</code> (configurable per command via cwd).</p>
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs text-secondary">Write / upload a file</label>
          <Input value={path} className="mb-1.5 font-mono" onChange={(e) => setPath(e.target.value)} />
          <textarea value={content} onChange={(e) => setContent(e.target.value)} rows={4} className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-xs" placeholder="file contents…" />
          <div className="mt-1.5 flex gap-2">
            <Button variant="outline" onClick={mkdir} disabled={busy}>Create Directory</Button>
            <Button onClick={write} disabled={busy}>Write File</Button>
          </div>
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Read / download a file</label>
          <Input value={readPath} className="mb-1.5 font-mono" onChange={(e) => setReadPath(e.target.value)} />
          <pre className="h-[104px] overflow-auto rounded-md border border-border bg-subtle/40 p-2 font-mono text-xs">{readResult ?? <span className="text-muted">(read result appears here)</span>}</pre>
          <div className="mt-1.5 flex gap-2">
            <Button variant="outline" onClick={read} disabled={busy}>Read</Button>
            <Button variant="danger" onClick={del} disabled={busy}><Trash2 className="h-3.5 w-3.5" /> Delete path</Button>
          </div>
        </div>
      </div>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Ports / preview URLs
// ---------------------------------------------------------------------------
function PortsCard({ project, sandbox }: { project: string; sandbox: SandboxRecord }) {
  const [urls, setUrls] = useState<Record<number, string>>({});
  const [busy, setBusy] = useState<number | null>(null);
  const base = `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandbox.id}`;

  async function getUrl(port: number) {
    setBusy(port);
    try {
      const r = await apiGet<{ url: string }>(`${base}/domain?port=${port}`, { fresh: true });
      setUrls((u) => ({ ...u, [port]: r.url }));
    } catch (e) {
      toast(String(e).replace(/^Error:\s*/, ""), {});
    } finally {
      setBusy(null);
    }
  }

  if (!sandbox.ports.length) {
    return (
      <Card>
        <h3 className="mb-1 text-sm font-semibold">Ports / Preview URLs</h3>
        <p className="text-sm text-secondary">No ports exposed. Configure ports when creating the sandbox.</p>
      </Card>
    );
  }

  return (
    <Card>
      <h3 className="mb-1 text-sm font-semibold">Ports / Preview URLs</h3>
      <p className="mb-3 text-xs text-amber-500">Exposed ports are reachable on this node&apos;s public interface — only expose ports you intend to be public.</p>
      <div className="flex flex-col gap-2">
        {sandbox.ports.map((p) => (
          <div key={p} className="flex items-center justify-between gap-3 rounded-md border border-border px-3 py-2">
            <span className="font-mono text-sm">:{p}</span>
            {urls[p] ? (
              <a href={urls[p]} target="_blank" rel="noreferrer" className="truncate text-sm text-link hover:underline">{urls[p]}</a>
            ) : (
              <Button variant="outline" onClick={() => getUrl(p)} disabled={busy === p}>
                {busy === p ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : "Get preview URL"}
              </Button>
            )}
          </div>
        ))}
      </div>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Snapshots panel
// ---------------------------------------------------------------------------
function SnapshotsPanel({ project, sandboxId, onChanged }: { project: string; sandboxId: string; onChanged: () => void }) {
  const base = `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandboxId}`;
  const { data, refresh } = usePoll<{ snapshots: SandboxSnapshotRecord[] }>(`${base}/snapshots`, 5000);
  const [busy, setBusy] = useState(false);
  const snapshots = data?.snapshots ?? [];

  async function create() {
    setBusy(true);
    try {
      await apiSend("POST", `${base}/snapshots`, {});
      refresh();
      onChanged();
    } catch (e) {
      toast(String(e).replace(/^Error:\s*/, ""), {});
    } finally {
      setBusy(false);
    }
  }
  async function del(id: string) {
    await apiSend("DELETE", `${base}/snapshots/${id}`);
    refresh();
  }

  return (
    <Card>
      <div className="mb-3 flex items-center justify-between">
        <h3 className="text-sm font-semibold">Snapshots</h3>
        <Button variant="outline" onClick={create} disabled={busy}>{busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Plus className="h-3.5 w-3.5" />} Create Snapshot</Button>
      </div>
      {!snapshots.length ? (
        <p className="text-sm text-secondary">No snapshots yet.</p>
      ) : (
        <div className="divide-y divide-border">
          {snapshots.map((s) => (
            <div key={s.id} className="flex items-center justify-between gap-3 py-2 text-sm">
              <div className="min-w-0">
                <div className="truncate font-mono text-xs">{s.id}</div>
                <div className="text-xs text-secondary">
                  {s.status} · {s.size_bytes ? `${Math.round(s.size_bytes / 1024)} KB` : "size unknown"} · {timeAgo(s.created_at)} ago
                  {s.expires_at ? ` · expires ${timeAgo(s.expires_at)}` : ""}
                </div>
              </div>
              <Button variant="ghost" onClick={() => del(s.id)}><Trash2 className="h-3.5 w-3.5" /></Button>
            </div>
          ))}
        </div>
      )}
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Storage / Mounts panel
// ---------------------------------------------------------------------------
function MountsPanel({ project, sandboxId }: { project: string; sandboxId: string }) {
  const base = `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandboxId}`;
  const { data, refresh } = usePoll<{ mounts: SandboxMountRecord[] }>(`${base}/mounts`, 5000);
  const mounts = data?.mounts ?? [];
  const [showForm, setShowForm] = useState(false);

  const [mountPath, setMountPath] = useState("/mnt/data");
  const [kind, setKind] = useState<"drive" | "remote-fuse">("drive");
  const [mode, setMode] = useState<"read-only" | "read-write">("read-write");
  const [provider, setProvider] = useState("s3");
  const [bucket, setBucket] = useState("");
  const [region, setRegion] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [accessKey, setAccessKey] = useState("");
  const [secretKey, setSecretKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  async function create() {
    setErr("");
    if (kind === "remote-fuse" && (!accessKey || !secretKey)) {
      setErr("Access key and secret key are required for remote storage mounts.");
      return;
    }
    setBusy(true);
    try {
      await apiSend("POST", `${base}/mounts`, {
        mountPath,
        type: kind,
        mode,
        provider: kind === "drive" ? "platform" : provider,
        bucket,
        region,
        endpoint,
        accessKey,
        secretKey,
      });
      setShowForm(false);
      refresh();
    } catch (e) {
      setErr(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  }
  async function del(id: string) {
    await apiSend("DELETE", `${base}/mounts/${id}`);
    refresh();
  }

  return (
    <Card>
      <div className="mb-1 flex items-center justify-between">
        <h3 className="text-sm font-semibold">Storage / Mounts</h3>
        <Button variant="outline" onClick={() => setShowForm((v) => !v)}><Plus className="h-3.5 w-3.5" /> Add Mount</Button>
      </div>
      <p className="mb-3 text-xs text-amber-500">Sandbox filesystem persistence is not a replacement for a database or durable object store.</p>

      {!mounts.length ? (
        <p className="text-sm text-secondary">No mounts configured.</p>
      ) : (
        <div className="mb-3 divide-y divide-border">
          {mounts.map((m) => (
            <div key={m.id} className="flex items-center justify-between gap-3 py-2 text-sm">
              <div className="min-w-0">
                <div className="font-mono text-xs">{m.mount_path}</div>
                <div className="text-xs text-secondary">{m.type} · {m.mode} · {m.provider} · <Badge tone={m.status === "mounted" ? "green" : "amber"}>{m.status}</Badge></div>
                {m.note ? <div className="text-xs text-muted">{m.note}</div> : null}
              </div>
              <Button variant="ghost" onClick={() => del(m.id)}><Trash2 className="h-3.5 w-3.5" /></Button>
            </div>
          ))}
        </div>
      )}

      {showForm && (
        <div className="rounded-lg border border-border p-3">
          {err ? <div className="mb-2 rounded-md border border-red-500/30 bg-red-500/5 p-2 text-xs text-red-500">{err}</div> : null}
          <div className="mb-2 flex gap-2">
            <label className="flex items-center gap-1.5 text-sm"><input type="radio" checked={kind === "drive"} onChange={() => setKind("drive")} /> Drive (persistent volume)</label>
            <label className="flex items-center gap-1.5 text-sm"><input type="radio" checked={kind === "remote-fuse"} onChange={() => setKind("remote-fuse")} /> Remote storage (FUSE)</label>
          </div>
          <div className="grid grid-cols-2 gap-2">
            <div>
              <label className="mb-1 block text-xs text-secondary">Mount path</label>
              <Input value={mountPath} className="font-mono" onChange={(e) => setMountPath(e.target.value)} />
            </div>
            <div>
              <label className="mb-1 block text-xs text-secondary">Mode</label>
              <select className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm" value={mode} onChange={(e) => setMode(e.target.value as "read-only" | "read-write")}>
                <option value="read-write">Read-write</option>
                <option value="read-only">Read-only</option>
              </select>
            </div>
          </div>
          {kind === "remote-fuse" && (
            <>
              <div className="mt-2">
                <label className="mb-1 block text-xs text-secondary">Provider</label>
                <select className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm" value={provider} onChange={(e) => setProvider(e.target.value)}>
                  <option value="s3">S3</option>
                  <option value="r2">R2</option>
                  <option value="gcs">GCS</option>
                  <option value="custom">Custom</option>
                </select>
              </div>
              <div className="mt-2 grid grid-cols-2 gap-2">
                <Input value={bucket} placeholder="bucket / container" onChange={(e) => setBucket(e.target.value)} />
                <Input value={region} placeholder="region" onChange={(e) => setRegion(e.target.value)} />
              </div>
              <div className="mt-2">
                <Input value={endpoint} placeholder="endpoint (optional, for custom/R2)" onChange={(e) => setEndpoint(e.target.value)} />
              </div>
              <div className="mt-2 grid grid-cols-2 gap-2">
                <Input value={accessKey} placeholder="access key" type="password" onChange={(e) => setAccessKey(e.target.value)} />
                <Input value={secretKey} placeholder="secret key" type="password" onChange={(e) => setSecretKey(e.target.value)} />
              </div>
              <p className="mt-1.5 text-xs text-muted">Credentials are sealed with the platform secret manager — never stored in plaintext.</p>
            </>
          )}
          <div className="mt-3 flex justify-end gap-2">
            <Button variant="ghost" onClick={() => setShowForm(false)}>Cancel</Button>
            <Button onClick={create} disabled={busy}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : "Add Mount"}</Button>
          </div>
        </div>
      )}
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Network policy panel
// ---------------------------------------------------------------------------
function NetworkPolicyPanel({ project, sandboxId, sandbox, onChanged }: { project: string; sandboxId: string; sandbox: SandboxRecord; onChanged: () => void }) {
  const base = `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandboxId}`;
  const [mode, setMode] = useState<NetworkPolicy["mode"]>(sandbox.network_policy.mode);
  const [allowedDomains, setAllowedDomains] = useState(sandbox.network_policy.allowed_domains.join(", "));
  const [allowedSubnets, setAllowedSubnets] = useState(sandbox.network_policy.allowed_subnets.join(", "));
  const [deniedSubnets, setDeniedSubnets] = useState(sandbox.network_policy.denied_subnets.join(", "));
  const [proxy, setProxy] = useState(sandbox.network_policy.forward_proxy ?? "");
  const [busy, setBusy] = useState(false);

  const dirty =
    mode !== sandbox.network_policy.mode ||
    allowedDomains !== sandbox.network_policy.allowed_domains.join(", ") ||
    allowedSubnets !== sandbox.network_policy.allowed_subnets.join(", ") ||
    deniedSubnets !== sandbox.network_policy.denied_subnets.join(", ") ||
    proxy !== (sandbox.network_policy.forward_proxy ?? "");

  async function save() {
    setBusy(true);
    try {
      await apiSend("PUT", `${base}/network-policy`, {
        mode,
        allowed_domains: allowedDomains.split(",").map((s) => s.trim()).filter(Boolean),
        allowed_subnets: allowedSubnets.split(",").map((s) => s.trim()).filter(Boolean),
        denied_subnets: deniedSubnets.split(",").map((s) => s.trim()).filter(Boolean),
        forward_proxy: proxy.trim() || null,
      });
      onChanged();
    } catch (e) {
      toast(String(e).replace(/^Error:\s*/, ""), {});
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <h3 className="mb-1 text-sm font-semibold">Network Policy</h3>
      <p className="mb-3 text-xs text-secondary">A policy change takes effect on this sandbox&apos;s next start (Firecracker has no live network-policy toggle for a running microVM).</p>
      <select className="mb-3 w-full rounded-md border border-border bg-card px-3 py-2 text-sm" value={mode} onChange={(e) => setMode(e.target.value as NetworkPolicy["mode"])}>
        <option value="allow-all">Allow all</option>
        <option value="deny-all">Deny all</option>
        <option value="allowlist">Allowlist</option>
      </select>
      {mode === "allowlist" && (
        <div className="mb-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-2.5 text-xs text-amber-600">
          Allowlist enforcement at the network layer is not yet wired up — this is validated and stored, but the sandbox currently gets full outbound access regardless. See project documentation.
        </div>
      )}
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs text-secondary">Allowed domains</label>
          <Input value={allowedDomains} placeholder="api.example.com" onChange={(e) => setAllowedDomains(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Allowed subnets</label>
          <Input value={allowedSubnets} placeholder="10.0.0.0/8" className="font-mono" onChange={(e) => setAllowedSubnets(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Denied subnets</label>
          <Input value={deniedSubnets} placeholder="169.254.0.0/16" className="font-mono" onChange={(e) => setDeniedSubnets(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Forward proxy (optional)</label>
          <Input value={proxy} placeholder="http://proxy:8080" onChange={(e) => setProxy(e.target.value)} />
        </div>
      </div>
      {sandbox.ports.length > 0 ? <p className="mt-3 text-xs text-amber-500">This sandbox exposes {sandbox.ports.length} port(s) publicly on the host node — see Ports above.</p> : null}
      <div className="mt-3 flex justify-end">
        <Button onClick={save} disabled={!dirty || busy}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : "Save"}</Button>
      </div>
    </Card>
  );
}
