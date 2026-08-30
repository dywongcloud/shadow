"use client";

import { useEffect, useState, use } from "react";
import { ChevronDown, ExternalLink } from "lucide-react";
import { Button, Switch, Input, SettingCard } from "@/components/ui";
import { RegionMap } from "@/components/region-map";
import { apiGet, apiSend, type FunctionSettings, type RegionCatalog } from "@/lib/api";
import { cn } from "@/lib/utils";

export function FunctionsSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const [fs, setFs] = useState<FunctionSettings | null>(null);
  const [catalog, setCatalog] = useState<RegionCatalog>({});
  const [maxDur, setMaxDur] = useState("300");
  const [open, setOpen] = useState<Record<string, boolean>>({ "North America": true });
  const [plan, setPlan] = useState("hobby");

  const [err, setErr] = useState("");

  // Plan-tier limits (mirror the backend: billing::plan_max_duration_secs etc.).
  const planMax = plan === "enterprise" ? 3600 : plan === "pro" ? 800 : 300;
  const canFailover = plan === "enterprise";

  async function load() {
    try {
      const s = await apiGet<{ functions: FunctionSettings }>(
        `/v1/projects/${encodeURIComponent(project)}/settings`
      );
      const fallback: FunctionSettings = {
        fluid_enabled: true,
        default_max_duration_secs: 300,
        // No hard-coded region — real regions come from the live mesh catalog.
        regions: [],
        failover: false,
        // Standard serverless tier: 1 vCPU / 2 GB.
        vcpus: 1,
        memory_mib: 2048,
        gpu: false,
      };
      const fn = { ...fallback, ...(s.functions ?? {}) };
      setFs(fn);
      setMaxDur(String(fn.default_max_duration_secs));
      setErr("");
    } catch (e) {
      setErr(String(e));
    }
  }
  useEffect(() => {
    load();
    apiGet<RegionCatalog>("/v1/regions/catalog").then(setCatalog).catch(() => {});
    apiGet<{ account?: { plan?: string } }>("/v1/billing").then((d) => setPlan(d.account?.plan || "hobby")).catch(() => {});
  }, [project]);

  async function save(next: FunctionSettings) {
    setFs(next);
    await apiSend("PUT", `/v1/projects/${project}/functions`, next);
  }

  if (err) return <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-4 text-sm text-red-500">Failed to load function settings: {err}</div>;
  if (!fs) return <div className="text-sm text-secondary">Loading…</div>;

  function toggleRegion(id: string) {
    const has = fs!.regions.includes(id);
    const regions = has ? fs!.regions.filter((r) => r !== id) : [...fs!.regions, id];
    save({ ...fs!, regions });
  }

  // Coordinates for every catalog region, so selected regions plot on the map at
  // their REAL location (driven by where the backing node actually is).
  const coordById: Record<string, { lat: number; lon: number; label: string }> = {};
  Object.values(catalog).forEach((regions) =>
    regions.forEach((r) => {
      if (typeof r.lat === "number" && typeof r.lon === "number") {
        coordById[r.id] = { lat: r.lat, lon: r.lon, label: r.label };
      }
    })
  );
  const labelById: Record<string, string> = {};
  Object.values(catalog).forEach((regions) => regions.forEach((r) => { labelById[r.id] = r.label; }));
  const mapMarkers = fs.regions
    .map((id) => (coordById[id] ? { id, ...coordById[id] } : null))
    .filter((m): m is { id: string; lat: number; lon: number; label: string } => Boolean(m));

  return (
    <div className="flex flex-col gap-6">
      {/* Fluid Compute */}
      <SettingCard
        title="Fluid Compute"
        desc="Enable Fluid compute for your functions to automatically manage concurrency and optimize performance. DevHub handles the defaults to ensure the best experience for your workload."
        footer="A new deployment is required for changes to take effect."
        footerAction={<a className="text-sm text-link hover:underline" href="/usage">View Fluid compute metrics →</a>}
      >
        <div className="flex items-center gap-3">
          <Switch checked={fs.fluid_enabled} onChange={(v) => save({ ...fs, fluid_enabled: v })} label="Fluid compute" />
          <span className="text-sm font-medium">{fs.fluid_enabled ? "Enabled" : "Disabled"}</span>
        </div>
      </SettingCard>
      {/* Serverless GPU — placement targets GPU nodes and passes the host GPUs through. */}
      <SettingCard
        title="Serverless GPU"
        desc="Run this project's functions on GPU-equipped infrastructure (NVIDIA Tesla T4). Deployments are placed only on nodes with GPUs, and the GPUs are passed through to your function's container."
        footer="A new deployment is required for changes to take effect. GPU usage is metered."
      >
        <div className="flex items-center gap-3">
          <Switch checked={!!fs.gpu} onChange={(v) => save({ ...fs, gpu: v })} label="Serverless GPU" />
          <span className="text-sm font-medium">{fs.gpu ? "Enabled" : "Disabled"}</span>
        </div>
      </SettingCard>
      {/* CPU & Memory tier — one microVM per instance. */}
      <SettingCard
        title="CPU & Memory"
        desc="The resources each function instance (microVM) is allocated. Standard fits most serverless functions; Performance gives latency-sensitive apps and SSR workloads more headroom. A new deployment is required for changes to take effect."
        footer="Billed per active instance-time — Performance instances cost roughly 2× Standard."
      >
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          {(
            [
              { id: "standard", name: "Standard", vcpus: 1, mem: 2048, blurb: "1 vCPU · 2 GB memory — the default for serverless functions." },
              { id: "performance", name: "Performance", vcpus: 2, mem: 4096, blurb: "2 vCPUs · 4 GB memory — for latency-sensitive apps and SSR workloads." },
            ] as const
          ).map((t) => {
            const selected = (fs.vcpus ?? 1) >= 2 ? t.id === "performance" : t.id === "standard";
            return (
              <button
                key={t.id}
                onClick={() => save({ ...fs, vcpus: t.vcpus, memory_mib: t.mem })}
                className={cn(
                  "flex flex-col items-start gap-1 rounded-xl border p-4 text-left transition-colors",
                  selected ? "border-link bg-link/5 ring-1 ring-link" : "border-border hover:bg-subtle"
                )}
              >
                <div className="flex w-full items-center justify-between">
                  <span className="text-sm font-medium text-fg">{t.name}</span>
                  <span
                    className={cn(
                      "flex h-4 w-4 items-center justify-center rounded-full border text-[10px]",
                      selected ? "border-link bg-link text-white" : "border-border-strong"
                    )}
                  >
                    {selected ? "✓" : ""}
                  </span>
                </div>
                <span className="font-mono text-sm text-fg">{t.vcpus} vCPU{t.vcpus > 1 ? "s" : ""} · {t.mem / 1024} GB</span>
                <span className="text-xs text-secondary">{t.blurb}</span>
              </button>
            );
          })}
        </div>
      </SettingCard>
      {/* Function Max Duration */}
      <SettingCard
        title="Function Max Duration"
        desc={
          <>
            This setting controls the default maximum duration for all functions in this project. You can
            optionally override it at the function level. A new deployment is required for changes to take effect.
          </>
        }
        footer={`Hobby max 300s · Pro max 800s · Enterprise max 3600s (1 hour). Your plan (${plan}) allows up to ${planMax}s.`}
        footerAction={
          <Button
            onClick={() => save({ ...fs, default_max_duration_secs: Math.min(planMax, Math.max(1, parseInt(maxDur || "300", 10))) })}
          >
            Save
          </Button>
        }
      >
        <div className="rounded-lg border border-border bg-subtle p-4 text-sm text-secondary">
          <span className="font-medium text-fg">Longer functions:</span> {plan === "enterprise"
            ? "your Enterprise plan supports function runtimes up to 1 hour (3600s)."
            : <>upgrade to <span className="font-medium text-fg">Enterprise</span> for 1-hour (3600s) function runtimes. Your current cap is {planMax}s.</>}
        </div>
        <div className="mt-4">
          <label className="mb-1.5 block text-sm text-secondary">Default Max Duration</label>
          <div className="flex w-64 overflow-hidden rounded-md border border-border">
            <input
              value={maxDur}
              onChange={(e) => setMaxDur(e.target.value.replace(/[^0-9]/g, ""))}
              className="w-full bg-card px-3 py-2 text-sm focus:outline-none"
            />
            <span className="flex items-center border-l border-border bg-subtle px-3 text-sm text-secondary">seconds</span>
          </div>
        </div>
      </SettingCard>
      {/* Deployment Regions */}
      <SettingCard
        title="Deployment Regions"
        desc="The regions on the DevHub network where this project is deployed — every deployment (functions, containers, or static) is placed on the nodes in the regions you select, and requests are routed to the nearest one. Leave empty to deploy automatically to the eligible region nearest you. Select up to 5; redeploy for changes to take effect."
        footer={`${fs.regions.length}/5 regions selected${fs.regions.length === 0 ? " — auto (nearest region)" : ""}`}
      >
        {/* Global region map — dots mark the regions you've selected. Background and
            land follow the current UI theme (light/dark) rather than being hard-black. */}
        <div className="mx-auto mb-6 w-1/2">
          <div className="relative overflow-hidden rounded-xl border border-border bg-slate-50 dark:bg-[#070b14]">
            <RegionMap markers={mapMarkers} />
            <div className="absolute bottom-2 left-3 flex items-center gap-1.5 text-[11px] text-secondary">
              <span className="flex h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-400" />
              {fs.regions.length} active region{fs.regions.length === 1 ? "" : "s"} on your network
            </div>
          </div>
        </div>

        {/* Selected regions (so any selected-but-offline region is still visible
            and removable, even when no live node currently backs it). */}
        {fs.regions.length > 0 && (
          <div className="mb-4 flex flex-wrap gap-2">
            {fs.regions.map((id) => (
              <button
                key={id}
                onClick={() => toggleRegion(id)}
                className="group inline-flex items-center gap-1.5 rounded-full border border-link/30 bg-link/5 px-2.5 py-1 text-xs text-link"
                title="Remove region"
              >
                {labelById[id] || id}
                <span className="text-link/60 group-hover:text-link">✕</span>
              </button>
            ))}
          </div>
        )}
        {Object.keys(catalog).length === 0 && (
          <div className="mb-4 rounded-lg border border-border bg-subtle p-3 text-sm text-secondary">
            No regions detected yet — regions appear here as P2P nodes join the mesh and report their location.
          </div>
        )}
        <div className="flex flex-col divide-y divide-border">
          {Object.entries(catalog).map(([continent, regions]) => {
            const expanded = open[continent] ?? true;
            const selectedHere = regions.filter((r) => fs.regions.includes(r.id)).map((r) => r.id);
            return (
              <div key={continent} className="py-2">
                <button
                  className="flex w-full items-center justify-between py-1.5 text-sm font-medium"
                  onClick={() => setOpen((o) => ({ ...o, [continent]: !expanded }))}
                >
                  <span className="flex items-center gap-1.5">
                    <ChevronDown className={cn("h-4 w-4 transition-transform", expanded ? "" : "-rotate-90")} />
                    {continent}
                  </span>
                  {selectedHere.length > 0 && (
                    <span className="rounded-full bg-subtle px-2 py-0.5 text-xs text-secondary">
                      {selectedHere.join(", ")}
                    </span>
                  )}
                </button>
                {expanded && (
                  <div className="mt-2 grid grid-cols-1 gap-2 sm:grid-cols-2">
                    {regions.map((r) => {
                      const sel = fs.regions.includes(r.id);
                      return (
                        <button
                          key={r.id}
                          onClick={() => toggleRegion(r.id)}
                          className={cn(
                            "flex items-center justify-between rounded-lg border px-3 py-2.5 text-left text-sm transition-colors",
                            sel ? "border-link bg-link/5 text-link" : "border-border hover:bg-subtle"
                          )}
                        >
                          <span>
                            {r.label} <span className="text-muted">· {r.id}</span>
                            {r.nodes ? <span className="text-muted"> · {r.nodes} node{r.nodes === 1 ? "" : "s"}</span> : null}
                          </span>
                          <span
                            className={cn(
                              "flex h-4 w-4 items-center justify-center rounded border",
                              sel ? "border-link bg-link text-white" : "border-border-strong"
                            )}
                          >
                            {sel ? "✓" : ""}
                          </span>
                        </button>
                      );
                    })}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      </SettingCard>
      {/* Function Failover — automatic multi-region fail-over is Enterprise-only. */}
      <SettingCard
        title="Automatic Region Fail-over"
        desc="Automatically fail functions over to the nearest healthy region when one becomes unavailable. A new deployment is required for changes to take effect."
        footer={canFailover ? "Enterprise: automatic multi-region fail-over." : "Available on the Enterprise plan."}
      >
        <div className="flex items-center gap-3">
          <Switch
            checked={fs.failover}
            disabled={!canFailover}
            onChange={(v) => canFailover && save({ ...fs, failover: v })}
            label="Failover"
          />
          <span className="text-sm font-medium">{fs.failover ? "Enabled" : "Disabled"}</span>
          {!canFailover && (
            <span className="rounded-full border border-amber-500/30 bg-amber-500/10 px-2 py-0.5 text-[11px] font-medium text-amber-600 dark:text-amber-400">Enterprise</span>
          )}
        </div>
      </SettingCard>
    </div>
  );
}
