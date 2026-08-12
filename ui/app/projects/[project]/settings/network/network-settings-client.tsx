"use client";

import { use, useEffect, useMemo, useState } from "react";
import { Globe, Loader2, Plus, Server, Wifi, X, Zap } from "lucide-react";
import { Badge, Button, SettingCard } from "@/components/ui";
import { RawPortConnections } from "@/components/raw-port-connections";
import { apiSend, usePoll, type Deployment, type ProjectSettings, type RawPortBinding } from "@/lib/api";
import { deploymentHost } from "@/lib/deploy-url";
import { timeAgo } from "@/lib/utils";

/** One editable port row (string-typed while being edited; validated on save). */
interface PortRow {
  container_port: string;
  protocol: string;
  label: string;
}

/** Wire shape of `PUT /v1/projects/:project/network`'s response. */
interface NetworkPutResp {
  project: string;
  deployment: string;
  ports: Array<{ container_port: number; protocol: string; label?: string; public_port?: number }>;
  raw_ports: RawPortBinding[];
}

const PROTOCOLS = ["http", "tcp", "udp", "grpc"] as const;

/**
 * Network settings for a project: the production HTTP domain plus an EDITABLE
 * exposed-ports list for raw-protocol services (Postgres, Minecraft, gRPC —
 * anything with no Host header to route on). Saving PUTs
 * `/v1/projects/:project/network`, which rewrites the current production
 * deployment's port specs IN PLACE and allocates a public port per raw spec —
 * no redeploy: the raw proxy / UDP relay pick the new binding up from the
 * updated record within seconds. The allocated `host:port` connection
 * addresses render below the editor (same `RawPortConnections` block the
 * deployment-detail page uses).
 */
export function NetworkSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const { data: deps } = usePoll<Deployment[]>("/deployments", 5000);
  // The purchase record (survives across redeploys) — separate from the
  // deployment-stamped `dedicated_ipv4` below, which only appears once a
  // deploy AFTER purchase actually attaches the address. Polled so a return
  // from Stripe checkout (or the webhook completing) reflects here with no
  // manual refresh.
  const { data: pSettings, refresh: refreshSettings } = usePoll<ProjectSettings>(
    `/v1/projects/${encodeURIComponent(project)}/settings`,
    5000
  );

  const dep = useMemo(() => {
    const mine = (deps ?? []).filter((d) => d.project === project);
    return mine.find((d) => d.production) ?? mine.sort((a, b) => b.created_at_ms - a.created_at_ms)[0] ?? null;
  }, [deps, project]);

  const [rows, setRows] = useState<PortRow[]>([]);
  // Which deployment the rows were seeded from — reseed only when it changes
  // (or after a save), never on a background poll tick mid-edit.
  const [seededFor, setSeededFor] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState("");
  // Bindings straight from the save response, so the allocated public port
  // shows immediately instead of waiting for the next /deployments poll.
  const [savedBindings, setSavedBindings] = useState<RawPortBinding[] | null>(null);

  useEffect(() => {
    if (!dep || dep.id === seededFor || dirty) return;
    // Seed the editor from the stamped raw bindings (the only per-port shape
    // the deployment list carries). An HTTP-only deployment starts empty.
    setRows(
      (dep.raw_ports ?? []).map((b) => ({
        container_port: String(b.container_port),
        protocol: b.protocol,
        label: b.label ?? "",
      }))
    );
    setSeededFor(dep.id);
  }, [dep, seededFor, dirty]);

  const host = dep ? deploymentHost(dep.alias, dep.region_code) : "";
  const displayDep = dep ? { ...dep, raw_ports: savedBindings ?? dep.raw_ports } : null;
  const hasRawPorts = !!displayDep?.raw_ports?.length;

  // Dedicated IPv4: two independent signals, deliberately not collapsed into
  // one. `purchasedIp` is the durable purchase record (ProjectSettings —
  // survives redeploys); `activeIp` is what's actually stamped onto the
  // CURRENT deployment (only true once a deploy after purchase attaches it).
  // Purchased-but-not-yet-attached is a real, honest "Pending" state, never
  // shown as either "none" or "active".
  const purchasedIp = pSettings?.dedicated_ipv4 ?? null;
  const activeIp = displayDep?.dedicated_ipv4 ?? null;
  const [ipBusy, setIpBusy] = useState(false);
  const [ipErr, setIpErr] = useState("");

  async function buyDedicatedIpv4() {
    setIpErr("");
    setIpBusy(true);
    try {
      // Mirrors billing/page.tsx's checkout() exactly — same endpoint, same
      // response shape, same applied/url branching (an addon's price is
      // never zero, but the "applied" case is handled anyway rather than
      // assumed away).
      const r = await apiSend<{ url: string; mock: boolean; applied?: boolean }>(
        "POST",
        "/v1/billing/checkout",
        { kind: "addon", sku: "dedicated_ipv4", target: project }
      );
      if (r.applied) {
        setIpBusy(false);
        refreshSettings();
        return;
      }
      window.location.href = r.url;
    } catch (e) {
      setIpErr(String(e instanceof Error ? e.message : e));
      setIpBusy(false);
    }
  }

  function edit(i: number, patch: Partial<PortRow>) {
    setRows((r) => r.map((row, n) => (n === i ? { ...row, ...patch } : row)));
    setDirty(true);
  }
  function addRow() {
    setRows((r) => [...r, { container_port: "", protocol: "tcp", label: "" }]);
    setDirty(true);
  }
  function removeRow(i: number) {
    setRows((r) => r.filter((_, n) => n !== i));
    setDirty(true);
  }

  async function save() {
    setErr("");
    const ports: Array<{ container_port: number; protocol: string; label?: string }> = [];
    for (const r of rows) {
      const p = parseInt(r.container_port, 10);
      if (!Number.isInteger(p) || p < 1 || p > 65535) {
        setErr(`"${r.container_port || ""}" is not a valid port (1-65535)`);
        return;
      }
      ports.push({ container_port: p, protocol: r.protocol, ...(r.label.trim() ? { label: r.label.trim() } : {}) });
    }
    setSaving(true);
    try {
      const resp = await apiSend<NetworkPutResp>("PUT", `/v1/projects/${encodeURIComponent(project)}/network`, { ports });
      setSavedBindings(resp.raw_ports ?? []);
      // Reseed from the server's answer (it carries http rows + stamped ports).
      setRows(
        (resp.ports ?? []).map((s) => ({
          container_port: String(s.container_port),
          protocol: s.protocol,
          label: s.label ?? "",
        }))
      );
      setDirty(false);
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="flex flex-col gap-6">
      <SettingCard title="Domain" desc="The production HTTP address for this project.">
        {host ? (
          <span className="inline-flex items-center gap-1.5 font-mono text-sm">
            <Globe className="h-3.5 w-3.5 shrink-0 text-muted" /> {host}
          </span>
        ) : (
          <span className="text-sm text-muted">No production deployment yet.</span>
        )}
      </SettingCard>

      <SettingCard
        title="Dedicated IPv4"
        desc="A real, static public IPv4 address for this project — useful when a downstream service needs to allowlist a fixed address (a partner API, a firewalled database, some game clients)."
        footer={
          activeIp
            ? "Attached to the current production deployment. It's re-adopted automatically on every future redeploy — never re-purchased."
            : purchasedIp
            ? "Purchased — it attaches automatically on this project's next build. Push a commit, or redeploy from the Deployments page, to activate it now."
            : undefined
        }
      >
        {!pSettings ? (
          <span className="text-sm text-muted">Loading…</span>
        ) : activeIp ? (
          <div className="flex flex-col gap-2">
            <div className="flex flex-wrap items-center gap-2">
              <Badge tone="green">Active</Badge>
              <span className="inline-flex items-center gap-1.5 font-mono text-sm">
                <Wifi className="h-3.5 w-3.5 shrink-0 text-muted" /> {activeIp.address}
              </span>
            </div>
            <div className="text-xs text-secondary">
              {activeIp.region ? `${activeIp.region} · ` : ""}
              <span className="font-mono">{activeIp.tencent_eip_id}</span>
              {activeIp.allocated_ms ? ` · allocated ${timeAgo(activeIp.allocated_ms)}` : ""}
            </div>
          </div>
        ) : purchasedIp ? (
          <div className="flex flex-col gap-2">
            <div className="flex flex-wrap items-center gap-2">
              <Badge tone="amber">Pending</Badge>
              <span className="inline-flex items-center gap-1.5 font-mono text-sm">
                <Wifi className="h-3.5 w-3.5 shrink-0 text-muted" /> {purchasedIp.address}
              </span>
            </div>
            <div className="text-xs text-secondary">
              {purchasedIp.region ? `${purchasedIp.region} · ` : ""}
              <span className="font-mono">{purchasedIp.tencent_eip_id}</span>
            </div>
          </div>
        ) : (
          <div className="flex flex-col gap-3">
            <span className="text-sm text-muted">No dedicated IPv4 purchased for this project yet.</span>
            {ipErr && <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-3 text-sm text-red-500">{ipErr}</div>}
            <div>
              <Button onClick={buyDedicatedIpv4} disabled={ipBusy}>
                {ipBusy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Zap className="h-4 w-4" />}
                Buy Dedicated IPv4
              </Button>
            </div>
          </div>
        )}
      </SettingCard>

      <SettingCard
        title="Exposed Ports"
        desc="A raw-protocol service (a database, a game server like Minecraft) has no Host header to route on, so it's reached through its own allocated public port instead — declare the port your container listens on and its protocol, and the platform allocates a public port for it. HTTP ports ride the shared domain above and need no allocation."
        footer="Changes apply to the current deployment in place — no redeploy. A raw port keeps its allocated public port across edits and redeploys. New UDP ports reach instances started after the change."
        footerAction={
          <Button onClick={save} disabled={saving || !dep}>
            {saving ? "Saving…" : "Save"}
          </Button>
        }
      >
        {!dep ? (
          <span className="text-sm text-muted">No deployment yet — deploy first, then expose ports here.</span>
        ) : (
          <div className="flex flex-col gap-3">
            {rows.length === 0 && (
              <span className="inline-flex items-center gap-1.5 text-sm text-muted">
                <Server className="h-3.5 w-3.5 shrink-0" /> No ports declared — this deployment speaks HTTP only.
              </span>
            )}
            {rows.map((r, i) => (
              <div key={i} className="flex flex-wrap items-center gap-2">
                <input
                  value={r.container_port}
                  onChange={(e) => edit(i, { container_port: e.target.value.replace(/[^0-9]/g, "") })}
                  placeholder="Port"
                  inputMode="numeric"
                  className="w-24 rounded-md border border-border bg-card px-3 py-2 font-mono text-sm focus:outline-none focus:ring-1 focus:ring-link"
                />
                <select
                  value={r.protocol}
                  onChange={(e) => edit(i, { protocol: e.target.value })}
                  className="rounded-md border border-border bg-card px-2 py-2 text-sm focus:outline-none focus:ring-1 focus:ring-link"
                >
                  {PROTOCOLS.map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))}
                  {/* Preserve a protocol outside the editable set (ws/wss/json-rpc from a config file). */}
                  {!PROTOCOLS.includes(r.protocol as (typeof PROTOCOLS)[number]) && <option value={r.protocol}>{r.protocol}</option>}
                </select>
                <input
                  value={r.label}
                  onChange={(e) => edit(i, { label: e.target.value })}
                  placeholder="Label (optional, e.g. game / rcon)"
                  className="w-56 rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none focus:ring-1 focus:ring-link"
                />
                <button
                  type="button"
                  onClick={() => removeRow(i)}
                  title="Remove port"
                  className="rounded-md border border-border p-2 text-muted transition-colors hover:bg-subtle hover:text-fg"
                >
                  <X className="h-3.5 w-3.5" />
                </button>
              </div>
            ))}
            <div>
              <button
                type="button"
                onClick={addRow}
                className="inline-flex items-center gap-1.5 rounded-md border border-border px-3 py-2 text-sm text-secondary transition-colors hover:bg-subtle hover:text-fg"
              >
                <Plus className="h-3.5 w-3.5" /> Add port
              </button>
            </div>
            {err && <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-3 text-sm text-red-500">{err}</div>}
          </div>
        )}
      </SettingCard>

      <SettingCard
        title="Raw TCP / UDP connections"
        desc="Ready-to-copy connection addresses for every allocated public port — connect with a plain host:port, same as any Postgres/Redis client or a game client's Server Address field."
      >
        {!displayDep ? (
          <span className="text-sm text-muted">No deployment yet.</span>
        ) : hasRawPorts ? (
          <RawPortConnections deployment={displayDep} />
        ) : (
          <span className="inline-flex items-center gap-1.5 text-sm text-muted">
            <Server className="h-3.5 w-3.5 shrink-0" /> No raw ports allocated — add a tcp/udp/grpc port above and save.
          </span>
        )}
      </SettingCard>
    </div>
  );
}
