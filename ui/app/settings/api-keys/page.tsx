"use client";

import { useState } from "react";
import Link from "next/link";
import { KeyRound, Plus, Trash2, Copy, Check, ArrowLeft, TriangleAlert } from "lucide-react";
import { Badge, Button, Card } from "@/components/ui";
import { apiGet, apiSend, currentTeam, usePoll, type ApiKey } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function ApiKeysPage() {
  const { data: keys, refresh } = usePoll<ApiKey[]>("/v1/apikeys", 5000);
  const [name, setName] = useState("");
  const [role, setRole] = useState("member");
  const [created, setCreated] = useState<ApiKey | null>(null);
  const [copied, setCopied] = useState(false);
  const [busy, setBusy] = useState(false);

  async function create() {
    if (!name.trim()) return;
    setBusy(true);
    try {
      const key = await apiSend<ApiKey>("POST", "/v1/apikeys", { name, role });
      setCreated(key); // shows the token once
      setName("");
      refresh();
    } catch (e) { alert(String(e)); } finally { setBusy(false); }
  }
  async function revoke(id: string) {
    if (!confirm("Revoke this API key? Any client using it will lose access.")) return;
    await apiSend("DELETE", `/v1/apikeys/${id}`);
    refresh();
  }
  function copyToken() {
    if (created?.token) {
      navigator.clipboard.writeText(created.token);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    }
  }

  return (
    <div>
      <Link href="/settings" className="mb-3 inline-flex items-center gap-1.5 text-sm text-link hover:underline"><ArrowLeft className="h-4 w-4" /> Team Settings</Link>
      <h1 className="text-2xl font-semibold tracking-tight">API Keys</h1>
      <p className="mb-6 mt-1 text-sm text-secondary">
        Tenant-scoped tokens for programmatic access to the platform API. A key is scoped to the
        <span className="font-mono"> {currentTeam()}</span> team and authenticates requests as
        <code className="mx-1 rounded bg-subtle px-1 font-mono text-xs">Authorization: Bearer hive_…</code>.
      </p>

      {/* one-time token reveal */}
      {created?.token && (
        <Card className="mb-6 border-amber-500/40 bg-amber-500/[0.04]">
          <div className="mb-2 flex items-center gap-2 text-sm font-medium text-amber-600 dark:text-amber-400">
            <TriangleAlert className="h-4 w-4" /> Copy your API key now — it won't be shown again.
          </div>
          <div className="flex items-center gap-2">
            <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-card px-3 py-2 font-mono text-sm">{created.token}</code>
            <Button variant="outline" onClick={copyToken}>{copied ? <><Check className="h-4 w-4 text-green" /> Copied</> : <><Copy className="h-4 w-4" /> Copy</>}</Button>
            <Button variant="ghost" onClick={() => setCreated(null)}>Done</Button>
          </div>
        </Card>
      )}

      {/* create */}
      <Card className="mb-6">
        <div className="mb-3 text-sm font-medium">Create API Key</div>
        <div className="flex flex-col gap-3 sm:flex-row sm:items-end">
          <div className="flex-1">
            <label className="mb-1 block text-xs font-medium text-secondary">Name</label>
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="ci-deploy" className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none" />
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-secondary">Role</label>
            <select value={role} onChange={(e) => setRole(e.target.value)} className="rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none">
              <option value="owner">Owner</option>
              <option value="member">Member</option>
              <option value="viewer">Viewer</option>
            </select>
          </div>
          <Button onClick={create} disabled={busy}><Plus className="h-4 w-4" /> Create Key</Button>
        </div>
      </Card>

      {/* list */}
      {keys?.length ? (
        <div className="divide-y divide-border rounded-xl border border-border bg-card">
          {keys.map((k) => (
            <div key={k.id} className="flex items-center justify-between gap-3 px-4 py-3">
              <div className="flex items-center gap-3">
                <span className="flex h-8 w-8 items-center justify-center rounded-lg bg-subtle text-secondary"><KeyRound className="h-4 w-4" /></span>
                <div>
                  <div className="flex items-center gap-2 text-sm font-medium">{k.name} <Badge>{k.role}</Badge></div>
                  <div className="font-mono text-xs text-muted">{k.prefix} · {k.last_used_ms ? `last used ${timeAgo(k.last_used_ms)} ago` : "never used"}</div>
                </div>
              </div>
              <button onClick={() => revoke(k.id)} className="text-muted hover:text-red-500"><Trash2 className="h-4 w-4" /></button>
            </div>
          ))}
        </div>
      ) : (
        <Card className="py-12 text-center text-sm text-secondary">No API keys yet. Create one to access the platform API.</Card>
      )}
    </div>
  );
}
