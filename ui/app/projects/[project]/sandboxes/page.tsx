"use client";

import { useState } from "react";
import Link from "next/link";
import { Box, Plus, X, Loader2 } from "lucide-react";
import { Badge, Button, Card, Input, PageHeader, Table, Th, Td, Triangle } from "@/components/ui";
import { apiSend, usePoll, type SandboxRecord, type SandboxRuntime, type NetworkPolicy } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

const RUNTIMES: SandboxRuntime[] = ["node22", "node24", "node26", "python3.13"];

function statusTone(s: string): "green" | "amber" | "red" | "default" | "blue" {
  if (s === "running") return "green";
  if (s === "pending" || s === "stopping") return "amber";
  if (s === "failed") return "red";
  return "default";
}

function timeRemaining(expiresAt?: number | null): string {
  if (!expiresAt) return "—";
  const ms = expiresAt - Date.now();
  if (ms <= 0) return "expired";
  const mins = Math.floor(ms / 60000);
  if (mins < 60) return `${mins}m`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m`;
}

export default function SandboxesPage({ params }: { params: { project: string } }) {
  const project = decodeURIComponent(params.project);
  const { data, error, refresh } = usePoll<{ sandboxes: SandboxRecord[] }>(`/v1/projects/${encodeURIComponent(project)}/sandboxes`, 4000);
  const [open, setOpen] = useState(false);

  const sandboxes = data?.sandboxes ?? [];

  return (
    <div className="pb-24">
      <div className="mb-1 flex items-center gap-2 text-sm text-secondary">
        <Triangle className="h-4 w-4" />
        <Link href={`/projects/${encodeURIComponent(project)}`} className="hover:underline">{project}</Link>
      </div>
      <PageHeader
        title="Sandboxes"
        desc="Create isolated Linux environments for running commands, testing code, and executing untrusted workloads."
        action={<Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Create Sandbox</Button>}
      />

      {error && !sandboxes.length ? (
        <Card className="flex flex-col items-center gap-3 py-16 text-center">
          <Box className="h-8 w-8 text-muted" />
          <div className="text-sm font-medium">Couldn&apos;t load sandboxes</div>
          <p className="max-w-sm text-sm text-secondary">{String(error).replace(/^Error:\s*/, "")}</p>
          <Button variant="outline" onClick={refresh}>Retry</Button>
        </Card>
      ) : !sandboxes.length ? (
        <Card className="flex flex-col items-center gap-3 py-16 text-center">
          <Box className="h-8 w-8 text-muted" />
          <div className="text-sm font-medium">No sandboxes yet</div>
          <p className="max-w-sm text-sm text-secondary">Create an isolated microVM sandbox to run commands, test code, or execute untrusted workloads.</p>
          <Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Create Sandbox</Button>
        </Card>
      ) : (
        <Table>
          <thead>
            <tr>
              <Th>Name</Th>
              <Th>Status</Th>
              <Th>Runtime</Th>
              <Th>vCPUs</Th>
              <Th>Memory</Th>
              <Th>Timeout</Th>
              <Th>Persistent</Th>
              <Th>Region</Th>
              <Th>Tags</Th>
              <Th>Created</Th>
              <Th />
            </tr>
          </thead>
          <tbody>
            {sandboxes.map((s) => (
              <tr key={s.id} className="cursor-pointer hover:bg-subtle">
                <Td className="font-medium">
                  <Link href={`/projects/${encodeURIComponent(project)}/sandboxes/${s.id}`} className="flex items-center gap-2">
                    <Box className="h-4 w-4 text-muted" />
                    {s.name}
                  </Link>
                </Td>
                <Td><Badge tone={statusTone(s.status)}>{s.status}</Badge></Td>
                <Td className="font-mono text-xs">{s.runtime}</Td>
                <Td>{s.vcpus}</Td>
                <Td>{s.memory_mb} MB</Td>
                <Td>{timeRemaining(s.timeout_expires_at)}</Td>
                <Td>{s.persistent ? <Badge tone="blue">Yes</Badge> : <Badge tone="default">No</Badge>}</Td>
                <Td className="text-secondary">{s.region}</Td>
                <Td>
                  <div className="flex flex-wrap gap-1">
                    {s.tags.slice(0, 3).map((t) => (
                      <Badge key={t} tone="default">{t}</Badge>
                    ))}
                  </div>
                </Td>
                <Td className="text-secondary">{timeAgo(s.created_at)} ago</Td>
                <Td>
                  <RowActions project={project} sandbox={s} onChanged={refresh} />
                </Td>
              </tr>
            ))}
          </tbody>
        </Table>
      )}

      {open && (
        <CreateSandboxDialog
          project={project}
          onClose={() => setOpen(false)}
          onCreated={() => {
            setOpen(false);
            refresh();
          }}
        />
      )}
    </div>
  );
}

function RowActions({ project, sandbox, onChanged }: { project: string; sandbox: SandboxRecord; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  async function stop() {
    setBusy(true);
    try {
      await apiSend("POST", `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandbox.id}/stop`);
      onChanged();
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy(false);
    }
  }
  async function snapshot() {
    setBusy(true);
    try {
      await apiSend("POST", `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandbox.id}/snapshots`, {});
      onChanged();
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy(false);
    }
  }
  async function destroy() {
    if (!confirm(`Delete sandbox "${sandbox.name}"? This cannot be undone.`)) return;
    setBusy(true);
    try {
      await apiSend("DELETE", `/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandbox.id}`);
      onChanged();
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="flex items-center gap-1" onClick={(e) => e.stopPropagation()}>
      <Link href={`/projects/${encodeURIComponent(project)}/sandboxes/${sandbox.id}`}>
        <Button variant="ghost">Open</Button>
      </Link>
      {sandbox.status === "running" && (
        <Button variant="ghost" onClick={stop} disabled={busy}>
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : "Stop"}
        </Button>
      )}
      <Button variant="ghost" onClick={snapshot} disabled={busy}>Snapshot</Button>
      <Button variant="danger" onClick={destroy} disabled={busy}>Delete</Button>
    </div>
  );
}

function CreateSandboxDialog({ project, onClose, onCreated }: { project: string; onClose: () => void; onCreated: () => void }) {
  const [name, setName] = useState("");
  const [runtime, setRuntime] = useState<SandboxRuntime>("node22");
  const [vcpus, setVcpus] = useState(1);
  const [timeoutMin, setTimeoutMin] = useState(5);
  const [persistent, setPersistent] = useState(false);
  const [portsText, setPortsText] = useState("");
  const [policyMode, setPolicyMode] = useState<NetworkPolicy["mode"]>("allow-all");
  const [allowedDomains, setAllowedDomains] = useState("");
  const [envRows, setEnvRows] = useState<{ key: string; value: string }[]>([]);
  const [tagsText, setTagsText] = useState("");
  const [sourceKind, setSourceKind] = useState("empty");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  const ports = portsText.split(",").map((p) => p.trim()).filter(Boolean).map(Number).filter((n) => Number.isFinite(n) && n > 0 && n < 65536);

  async function create() {
    setErr("");
    if (!name.trim()) {
      setErr("Name is required");
      return;
    }
    setBusy(true);
    try {
      await apiSend("POST", `/v1/projects/${encodeURIComponent(project)}/sandboxes`, {
        name: name.trim(),
        runtime,
        vcpus,
        timeout_ms: timeoutMin * 60_000,
        persistent,
        ports,
        network_policy: {
          mode: policyMode,
          allowed_domains: policyMode === "allowlist" ? allowedDomains.split(",").map((d) => d.trim()).filter(Boolean) : [],
          allowed_subnets: [],
          denied_subnets: [],
        },
        env: envRows.filter((r) => r.key.trim()).map((r) => ({ key: r.key.trim(), value: r.value, sensitive: false })),
        tags: tagsText.split(",").map((t) => t.trim()).filter(Boolean),
        source_kind: sourceKind,
        source_ref: "",
      });
      onCreated();
    } catch (e) {
      setErr(String(e).replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="fixed inset-0 z-[200] flex items-center justify-center bg-black/50 p-4 backdrop-blur-sm" onClick={onClose}>
      <div role="dialog" aria-modal="true" className="flex max-h-[85vh] w-full max-w-lg flex-col rounded-2xl border border-border bg-card shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="flex items-center justify-between border-b border-border px-5 py-3">
          <h3 className="text-sm font-semibold">Create Sandbox</h3>
          <button aria-label="Close" onClick={onClose} className="text-secondary hover:text-fg"><X className="h-4 w-4" /></button>
        </div>
        <div className="flex-1 space-y-4 overflow-y-auto p-5">
          {err ? <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-2.5 text-sm text-red-500">{err}</div> : null}
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Name</label>
            <Input value={name} placeholder="my-sandbox" onChange={(e) => setName(e.target.value)} autoFocus />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Runtime</label>
              <select className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm" value={runtime} onChange={(e) => setRuntime(e.target.value as SandboxRuntime)}>
                {RUNTIMES.map((r) => (
                  <option key={r} value={r}>{r}</option>
                ))}
              </select>
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">vCPUs</label>
              <Input type="number" min={1} max={8} value={vcpus} onChange={(e) => setVcpus(Math.max(1, Number(e.target.value) || 1))} />
            </div>
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Timeout (minutes)</label>
              <Input type="number" min={1} value={timeoutMin} onChange={(e) => setTimeoutMin(Math.max(1, Number(e.target.value) || 1))} />
            </div>
            <div className="flex items-end pb-2">
              <label className="flex items-center gap-2 text-sm">
                <input type="checkbox" checked={persistent} onChange={(e) => setPersistent(e.target.checked)} />
                Persistent (snapshot on stop)
              </label>
            </div>
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Ports to expose (comma-separated)</label>
            <Input value={portsText} placeholder="3000, 8080" onChange={(e) => setPortsText(e.target.value)} className="font-mono" />
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Network policy</label>
            <select className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm" value={policyMode} onChange={(e) => setPolicyMode(e.target.value as NetworkPolicy["mode"])}>
              <option value="allow-all">Allow all</option>
              <option value="deny-all">Deny all</option>
              <option value="allowlist">Allowlist</option>
            </select>
            {policyMode === "allowlist" && (
              <Input className="mt-2" value={allowedDomains} placeholder="api.example.com, cdn.example.com" onChange={(e) => setAllowedDomains(e.target.value)} />
            )}
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Environment variables</label>
            <div className="flex flex-col gap-1.5">
              {envRows.map((r, i) => (
                <div key={i} className="flex gap-1.5">
                  <Input value={r.key} placeholder="KEY" className="font-mono" onChange={(e) => setEnvRows((rows) => rows.map((x, j) => (j === i ? { ...x, key: e.target.value } : x)))} />
                  <Input value={r.value} placeholder="value" className="font-mono" onChange={(e) => setEnvRows((rows) => rows.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))} />
                  <Button variant="ghost" onClick={() => setEnvRows((rows) => rows.filter((_, j) => j !== i))}><X className="h-4 w-4" /></Button>
                </div>
              ))}
              <button className="flex w-fit items-center gap-1 text-xs text-secondary hover:text-fg" onClick={() => setEnvRows((r) => [...r, { key: "", value: "" }])}>
                <Plus className="h-3.5 w-3.5" /> Add variable
              </button>
            </div>
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Tags (comma-separated)</label>
            <Input value={tagsText} placeholder="ci, preview" onChange={(e) => setTagsText(e.target.value)} />
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Source</label>
            <select className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm" value={sourceKind} onChange={(e) => setSourceKind(e.target.value)}>
              <option value="empty">Empty workspace</option>
              <option value="git">Git repository</option>
              <option value="tarball">Tarball</option>
              <option value="snapshot">Snapshot</option>
              <option value="project">Existing project source</option>
            </select>
          </div>
        </div>
        <div className="flex justify-end gap-2 border-t border-border p-4">
          <Button variant="ghost" onClick={onClose}>Cancel</Button>
          <Button onClick={create} disabled={busy}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />} Create Sandbox</Button>
        </div>
      </div>
    </div>
  );
}
