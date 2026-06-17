"use client";

import { useState } from "react";
import { Globe2, Server, Share2, ShieldCheck, Database, Network, Lock, Plus, X, Trash2, Loader2 } from "lucide-react";
import { Card, Badge, Button, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type NodeInfo, type Overview, type SecureLink } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

interface ClusterStatus { term: number; leader: string; is_leader: boolean; members: string[]; consensus: string }

export default function NetworkPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 3000);
  const { data: ov } = usePoll<Overview>("/v1/overview", 4000);
  const { data: cluster } = usePoll<ClusterStatus>("/v1/cluster", 3000);
  const regions = Array.from(new Set((nodes ?? []).map((n) => n.region))).sort();

  return (
    <div>
      <PageHeader
        title="Network"
        desc="Your nodes form a peer-to-peer mesh over iroh QUIC. Gateways reach instances by endpoint id — no public IPs, NAT traversal and relay fallback handled automatically."
      />

      <div className="mb-6 grid grid-cols-2 gap-4 md:grid-cols-4">
        <Stat icon={<Server className="h-4 w-4" />} label="Nodes" value={nodes?.length ?? "—"} />
        <Stat icon={<Globe2 className="h-4 w-4" />} label="Regions" value={regions.length || "—"} />
        <Stat icon={<Share2 className="h-4 w-4" />} label="Transport" value="iroh QUIC" />
        <Stat icon={<Database className="h-4 w-4" />} label="State store" value="replicated" />
      </div>

      {/* Mesh visualization */}
      <Card className="mb-6">
        <div className="mb-4 flex items-center gap-2 text-sm font-medium">
          <Network className="h-4 w-4" /> P2P Mesh
        </div>
        <div className="flex flex-wrap items-center justify-center gap-6 py-6">
          {(nodes ?? []).map((n, i) => (
            <div key={n.id} className="flex flex-col items-center gap-2">
              <div
                className={`flex h-16 w-16 items-center justify-center rounded-full border-2 ${
                  n.is_self ? "border-green bg-green/10 text-green" : "border-border-strong bg-subtle text-secondary"
                }`}
              >
                <Server className="h-6 w-6" />
              </div>
              <div className="text-center">
                <div className="text-sm font-medium">{n.name}</div>
                <Badge tone="blue">{n.region}</Badge>
              </div>
              <div className="font-mono text-[10px] text-muted">{(n.peer_id || n.id).slice(0, 10)}…</div>
            </div>
          ))}
          {!nodes?.length && <div className="text-sm text-secondary">Discovering nodes…</div>}
        </div>
        <p className="mt-2 text-center text-xs text-muted">
          Every node runs a gateway + Fluid pool. The function tunnel protocol rides iroh streams, so a
          gateway on one node can serve an instance on another, anywhere reachable.
        </p>
      </Card>

      {/* Architecture rows */}
      <div className="mb-6 grid grid-cols-1 gap-4 md:grid-cols-3">
        <Arch icon={<Share2 className="h-4 w-4" />} title="P2P transport (iroh)" desc="Multiplexed function tunnels over QUIC; dialed by public-key endpoint id with relay fallback." />
        <Arch icon={<Database className="h-4 w-4" />} title="Replicated state" desc="Platform records persist to disk and replicate across the mesh (guardian-db, iroh-native)." />
        <Card>
          <div className="mb-2 flex items-center gap-2 text-sm font-medium"><ShieldCheck className="h-4 w-4" /> Coordinated cluster</div>
          <p className="text-sm text-secondary">Nodes agree on a leader so deployments stay consistent across regions.</p>
          {cluster && (
            <div className="mt-3 flex flex-col gap-1.5 text-xs">
              <div className="flex justify-between"><span className="text-muted">Leader</span><span className="font-mono">{cluster.leader}{cluster.is_leader ? " (this)" : ""}</span></div>
              <div className="flex justify-between"><span className="text-muted">Term</span><span className="font-mono">{cluster.term}</span></div>
              <div className="flex justify-between"><span className="text-muted">Members</span><span className="font-mono">{cluster.members.length}</span></div>
            </div>
          )}
        </Card>
      </div>

      <SecureConnections />

      <div className="mb-2 text-sm font-medium text-fg">Nodes</div>
      <Table>
        <thead><tr><Th>Node</Th><Th>Region</Th><Th>Endpoint</Th><Th>Role</Th><Th>Seen</Th></tr></thead>
        <tbody>
          {(nodes ?? []).map((n) => (
            <tr key={n.id}>
              <Td className="font-medium">{n.name}</Td>
              <Td><Badge tone="blue">{n.region}</Badge></Td>
              <Td className="font-mono text-xs text-secondary">{n.public_url}</Td>
              <Td>{n.is_self ? <Badge tone="green">this node</Badge> : <Badge>peer</Badge>}</Td>
              <Td className="text-secondary">{timeAgo(n.last_seen_ms)} ago</Td>
            </tr>
          ))}
        </tbody>
      </Table>
      <Card className="mt-4 text-sm text-secondary">
        Join another MacBook to the mesh:{" "}
        <code className="font-mono text-xs">hive-cloud --region fra1 --name node-b --peer http://&lt;this-ip&gt;:8786</code>
      </Card>
    </div>
  );
}

function Stat({ icon, label, value }: { icon: React.ReactNode; label: string; value: React.ReactNode }) {
  return (
    <Card className="flex flex-col gap-1">
      <span className="flex items-center gap-1.5 text-xs font-medium uppercase tracking-wide text-muted">{icon}{label}</span>
      <span className="text-2xl font-semibold text-fg">{value}</span>
    </Card>
  );
}

function Arch({ icon, title, desc }: { icon: React.ReactNode; title: string; desc: string }) {
  return (
    <Card>
      <div className="mb-2 flex items-center gap-2 text-sm font-medium">{icon}{title}</div>
      <p className="text-sm text-secondary">{desc}</p>
    </Card>
  );
}

/** Secure compute: ephemeral WireGuard tunnels to private backends. */
function SecureConnections() {
  const { data: links, refresh } = usePoll<SecureLink[]>("/v1/securelinks", 4000);
  const [open, setOpen] = useState(false);

  async function remove(id: string) {
    await apiSend("DELETE", `/v1/securelinks/${id}`);
    refresh();
  }

  return (
    <div className="mb-6">
      <Card className="mb-3">
        <div className="flex items-start justify-between">
          <div>
            <div className="mb-1 flex items-center gap-2 text-sm font-medium"><Lock className="h-4 w-4" /> Secure Connections</div>
            <p className="max-w-2xl text-sm text-secondary">
              Reach private backends (a DB in a VPC) over an ephemeral WireGuard tunnel. Keys are
              short-lived and minted on demand by the Key Exchange Service — no long-lived secrets
              in your builds or functions.
            </p>
          </div>
          <Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Connect Backend</Button>
        </div>
      </Card>

      {!!links?.length && (
        <Table>
          <thead><tr><Th>Backend</Th><Th>Local address</Th><Th>Wired to</Th><Th>Status</Th><Th>Expires</Th><Th></Th></tr></thead>
          <tbody>
            {links.map((l) => (
              <tr key={l.id}>
                <Td className="font-mono text-xs">{l.target}</Td>
                <Td className="font-mono text-xs text-secondary">{l.local_addr}</Td>
                <Td className="text-xs">
                  {l.project ? <span className="font-mono">{l.project}.{l.env_var}</span> : <span className="text-muted">—</span>}
                </Td>
                <Td><Badge tone={l.status === "active" ? "green" : "default"}>{l.status}</Badge></Td>
                <Td className="text-secondary">{l.status === "active" ? `in ${Math.max(0, Math.round((l.expires_ms - Date.now()) / 60000))}m` : "—"}</Td>
                <Td><button onClick={() => remove(l.id)} className="text-muted hover:text-red-500"><Trash2 className="h-3.5 w-3.5" /></button></Td>
              </tr>
            ))}
          </tbody>
        </Table>
      )}

      {open && <ConnectModal onClose={() => setOpen(false)} onDone={() => { setOpen(false); refresh(); }} />}
    </div>
  );
}

function ConnectModal({ onClose, onDone }: { onClose: () => void; onDone: () => void }) {
  const [target, setTarget] = useState("");
  const [ttl, setTtl] = useState("900");
  const [project, setProject] = useState("");
  const [envVar, setEnvVar] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  async function connect() {
    if (!target.trim()) return;
    setBusy(true); setErr("");
    try {
      await apiSend("POST", "/v1/securelinks", {
        target,
        ttl_secs: Number(ttl) || 900,
        project: project || undefined,
        env_var: envVar || undefined,
      });
      onDone();
    } catch (e) { setErr(String(e)); setBusy(false); }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <div className="w-full max-w-md rounded-xl border border-border bg-card p-6 shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="mb-4 flex items-center justify-between"><h2 className="flex items-center gap-2 text-lg font-semibold"><Lock className="h-4 w-4" /> Connect Private Backend</h2><button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button></div>
        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Backend address (host:port)</label>
            <input value={target} onChange={(e) => setTarget(e.target.value)} placeholder="db.internal:5432" className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none" />
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Lease TTL (seconds)</label>
            <input value={ttl} onChange={(e) => setTtl(e.target.value.replace(/[^0-9]/g, ""))} className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none" />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Wire to project <span className="text-muted">(optional)</span></label>
              <input value={project} onChange={(e) => setProject(e.target.value)} placeholder="my-project" className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none" />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Env var</label>
              <input value={envVar} onChange={(e) => setEnvVar(e.target.value)} placeholder="DATABASE_URL" className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none" />
            </div>
          </div>
          <p className="text-xs text-muted">If set, the connector's local address is injected as that env var so the project's functions reach the backend through the tunnel.</p>
          {err && <p className="text-xs text-red-500">{err}</p>}
        </div>
        <div className="mt-6 flex justify-end gap-2">
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button onClick={connect} disabled={busy}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Lock className="h-4 w-4" />} Establish Tunnel</Button>
        </div>
      </div>
    </div>
  );
}
