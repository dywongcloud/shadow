"use client";

import { useCallback, useEffect, useState, use } from "react";
import { Button, Input, SettingCard } from "@/components/ui";
import { apiGet, apiSend, type RoutingConfig } from "@/lib/api";

export function RoutingSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const [cfg, setCfg] = useState<RoutingConfig | null>(null);

  // New-rule drafts.
  const [rdSource, setRdSource] = useState("");
  const [rdDest, setRdDest] = useState("");
  const [rdStatus, setRdStatus] = useState(308);
  const [rwSource, setRwSource] = useState("");
  const [rwDest, setRwDest] = useState("");

  const load = useCallback(async () => {
    setCfg(await apiGet<RoutingConfig>(`/v1/routing`));
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `project` isn't read by the request itself, but reloading on project switch is intended (server scopes /v1/routing by current session/team)
  }, [project]);
  useEffect(() => {
    // Fetch-on-mount/project-change: load() sets state only after its
    // internal await resolves, syncing external (server) config into React.
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch effect; state is set only after an internal await, not synchronously
    load();
  }, [load]);

  async function addRedirect() {
    if (!rdSource.trim() || !rdDest.trim()) return;
    await apiSend("POST", `/v1/routing/redirects`, { source: rdSource.trim(), destination: rdDest.trim(), status: rdStatus });
    setRdSource("");
    setRdDest("");
    load();
  }
  async function delRedirect(source: string) {
    await apiSend("POST", `/v1/routing/redirects/delete`, { source });
    load();
  }
  async function addRewrite() {
    if (!rwSource.trim() || !rwDest.trim()) return;
    await apiSend("POST", `/v1/routing/rewrites`, { source: rwSource.trim(), destination: rwDest.trim() });
    setRwSource("");
    setRwDest("");
    load();
  }
  async function delRewrite(source: string) {
    await apiSend("POST", `/v1/routing/rewrites/delete`, { source });
    load();
  }

  if (!cfg) return <div className="text-sm text-secondary">Loading…</div>;

  return (
    <div className="flex flex-col gap-6">
      <SettingCard
        title="Redirects"
        desc="Send visitors from one path to another with a 307 (temporary) or 308 (permanent) response. Evaluated before rewrites."
      >
        <div className="flex flex-col gap-3">
          {cfg.redirects.length === 0 && <p className="text-xs text-secondary">No redirects configured.</p>}
          {cfg.redirects.map((r) => (
            <div key={r.source} className="flex items-center gap-2 rounded-md border border-border bg-card px-3 py-2 text-sm">
              <code className="font-mono">{r.source}</code>
              <span className="text-secondary">→</span>
              <code className="font-mono">{r.destination}</code>
              <span className="ml-1 rounded bg-subtle px-1.5 py-0.5 text-xs text-secondary">{r.status}</span>
              <button className="ml-auto text-xs text-secondary hover:text-red" onClick={() => delRedirect(r.source)}>
                Remove
              </button>
            </div>
          ))}
          <div className="flex flex-wrap items-end gap-2">
            <Field label="Source">
              <Input value={rdSource} onChange={(e) => setRdSource(e.target.value)} placeholder="/old" className="font-mono" />
            </Field>
            <Field label="Destination">
              <Input value={rdDest} onChange={(e) => setRdDest(e.target.value)} placeholder="/new" className="font-mono" />
            </Field>
            <Field label="Status">
              <select
                value={rdStatus}
                onChange={(e) => setRdStatus(Number(e.target.value))}
                className="rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
              >
                <option value={308}>308 (permanent)</option>
                <option value={307}>307 (temporary)</option>
                <option value={301}>301</option>
                <option value={302}>302</option>
              </select>
            </Field>
            <Button onClick={addRedirect}>Add redirect</Button>
          </div>
        </div>
      </SettingCard>

      <SettingCard
        title="Rewrites"
        desc="Map a public path to a different internal path without changing the URL the visitor sees. A prefix source ending in / preserves the remainder."
      >
        <div className="flex flex-col gap-3">
          {cfg.rewrites.length === 0 && <p className="text-xs text-secondary">No rewrites configured.</p>}
          {cfg.rewrites.map((r) => (
            <div key={r.source} className="flex items-center gap-2 rounded-md border border-border bg-card px-3 py-2 text-sm">
              <code className="font-mono">{r.source}</code>
              <span className="text-secondary">→</span>
              <code className="font-mono">{r.destination}</code>
              <button className="ml-auto text-xs text-secondary hover:text-red" onClick={() => delRewrite(r.source)}>
                Remove
              </button>
            </div>
          ))}
          <div className="flex flex-wrap items-end gap-2">
            <Field label="Source">
              <Input value={rwSource} onChange={(e) => setRwSource(e.target.value)} placeholder="/proxy/" className="font-mono" />
            </Field>
            <Field label="Destination">
              <Input value={rwDest} onChange={(e) => setRwDest(e.target.value)} placeholder="/internal/" className="font-mono" />
            </Field>
            <Button onClick={addRewrite}>Add rewrite</Button>
          </div>
        </div>
      </SettingCard>

      <SettingCard
        title="Configured in vercel.json"
        desc="These are read from your repository's vercel.json (or vercel.ts) on each deployment and honored automatically — no dashboard setup needed."
      >
        <ul className="flex flex-col gap-1.5 text-sm text-secondary">
          <li>
            <code className="font-mono text-fg">redirects</code> / <code className="font-mono text-fg">rewrites</code> — with{" "}
            <code className="font-mono">:param</code>, regex, and <code className="font-mono">has</code>/<code className="font-mono">missing</code> conditions.
          </li>
          <li>
            <code className="font-mono text-fg">headers</code> — response headers injected per matching path.
          </li>
          <li>
            <code className="font-mono text-fg">cleanUrls</code> / <code className="font-mono text-fg">trailingSlash</code> — URL normalization (308).
          </li>
          <li>
            <code className="font-mono text-fg">images</code> — Image Optimization at <code className="font-mono">/_vercel/image</code> (sizes/qualities/remotePatterns enforced).
          </li>
          <li>
            <code className="font-mono text-fg">crons</code> — registered against the production deployment.
          </li>
          <li>
            <code className="font-mono text-fg">functions</code> — per-function memory, maxDuration, and regions.
          </li>
        </ul>
      </SettingCard>
    </div>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <label className="mb-1.5 block text-xs font-medium text-secondary">{label}</label>
      {children}
    </div>
  );
}
