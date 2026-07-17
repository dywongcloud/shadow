"use client";

import { useState, use } from "react";
import { Lock, Plus, X, Trash2, Loader2 } from "lucide-react";
import { Badge, Button, Card, SettingCard, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type SecureLink } from "@/lib/api";

export function SecureComputeSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const { data: all, refresh } = usePoll<SecureLink[]>("/v1/securelinks", 4000);
  const [open, setOpen] = useState(false);

  const links = (all ?? []).filter((l) => l.project === project);

  async function remove(id: string) {
    await apiSend("DELETE", `/v1/securelinks/${id}`);
    refresh();
  }

  return (
    <div className="space-y-6">
      <SettingCard
        title="Secure Compute"
        desc="Reach a private backend (a DB in a VPC) from this project's functions over an ephemeral WireGuard tunnel. Keys are short-lived and minted on demand — no long-lived secrets. The connector's local address is injected as an env var your functions can use."
        footer="Tunnels are scoped to this project and expire automatically."
        footerAction={<Button onClick={() => setOpen(true)}><Plus className="h-4 w-4" /> Connect Backend</Button>}
      >
        {links.length ? (
          <Table>
            <thead><tr><Th>Backend</Th><Th>Local address</Th><Th>Env var</Th><Th>Status</Th><Th>Expires</Th><Th></Th></tr></thead>
            <tbody>
              {links.map((l) => (
                <tr key={l.id}>
                  <Td className="font-mono text-xs">{l.target}</Td>
                  <Td className="font-mono text-xs text-secondary">{l.local_addr}</Td>
                  <Td className="font-mono text-xs">{l.env_var || <span className="text-muted">—</span>}</Td>
                  <Td><Badge tone={l.status === "active" ? "green" : "default"}>{l.status}</Badge></Td>
                  <Td className="text-secondary">{l.status === "active" ? `in ${Math.max(0, Math.round((l.expires_ms - Date.now()) / 60000))}m` : "—"}</Td>
                  <Td><button onClick={() => remove(l.id)} className="text-muted hover:text-red-500"><Trash2 className="h-3.5 w-3.5" /></button></Td>
                </tr>
              ))}
            </tbody>
          </Table>
        ) : (
          <div className="rounded-lg border border-dashed border-border py-10 text-center text-sm text-secondary">
            No secure tunnels for this project yet.
          </div>
        )}
      </SettingCard>

      {open && <ConnectModal project={project} onClose={() => setOpen(false)} onDone={() => { setOpen(false); refresh(); }} />}
    </div>
  );
}

function ConnectModal({ project, onClose, onDone }: { project: string; onClose: () => void; onDone: () => void }) {
  const [target, setTarget] = useState("");
  const [envVar, setEnvVar] = useState("DATABASE_URL");
  const [ttl, setTtl] = useState("900");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  async function connect() {
    if (!target.trim()) return;
    setBusy(true); setErr("");
    try {
      await apiSend("POST", "/v1/securelinks", {
        target,
        ttl_secs: Number(ttl) || 900,
        project,
        env_var: envVar || undefined,
      });
      onDone();
    } catch (e) { setErr(String(e)); setBusy(false); }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <div className="w-full max-w-md rounded-xl border border-border bg-card p-6 shadow-pop" onClick={(e) => e.stopPropagation()}>
        <div className="mb-4 flex items-center justify-between">
          <h2 className="flex items-center gap-2 text-lg font-semibold"><Lock className="h-4 w-4" /> Connect Private Backend</h2>
          <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
        </div>
        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Backend address (host:port)</label>
            <input value={target} onChange={(e) => setTarget(e.target.value)} placeholder="db.internal:5432" className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none" />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Inject as env var</label>
              <input value={envVar} onChange={(e) => setEnvVar(e.target.value)} placeholder="DATABASE_URL" className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none" />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-secondary">Lease TTL (s)</label>
              <input value={ttl} onChange={(e) => setTtl(e.target.value.replace(/[^0-9]/g, ""))} className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none" />
            </div>
          </div>
          <p className="text-xs text-muted">{project}.{envVar || "ENV"} will be set to the connector's local address.</p>
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
