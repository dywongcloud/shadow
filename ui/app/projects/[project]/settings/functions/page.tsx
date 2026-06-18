"use client";

import { useEffect, useState } from "react";
import { ChevronDown, ExternalLink } from "lucide-react";
import { Button, Switch, Input, SettingCard } from "@/components/ui";
import { apiGet, apiSend, type FunctionSettings, type RegionCatalog } from "@/lib/api";
import { cn } from "@/lib/utils";

export default function FunctionsSettings({ params }: { params: { project: string } }) {
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
        regions: ["iad1"],
        failover: false,
        memory_mib: 512,
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

  return (
    <div className="flex flex-col gap-6">
      {/* Fluid Compute */}
      <SettingCard
        title="Fluid Compute"
        desc="Enable Fluid compute for your functions to automatically manage concurrency and optimize performance. OpenEdge handles the defaults to ensure the best experience for your workload."
        footer="A new deployment is required for changes to take effect."
        footerAction={<a className="text-sm text-link hover:underline" href="/usage">View Fluid compute metrics →</a>}
      >
        <div className="flex items-center gap-3">
          <Switch checked={fs.fluid_enabled} onChange={(v) => save({ ...fs, fluid_enabled: v })} label="Fluid compute" />
          <span className="text-sm font-medium">{fs.fluid_enabled ? "Enabled" : "Disabled"}</span>
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

      {/* Function Regions */}
      <SettingCard
        title="Function Regions"
        desc="These are the regions on the OpenEdge network that your functions will execute in. You can use up to 5 regions on your current plan. A new deployment is required for changes to take effect."
        footer={`${fs.regions.length}/5 regions selected`}
      >
        {/* Global region map — your functions run close to your users. */}
        <div className="relative mb-6 overflow-hidden rounded-xl border border-border bg-black">
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img src="/world-dots.png" alt="OpenEdge global region network" className="w-full select-none object-cover opacity-95" />
          <div className="pointer-events-none absolute inset-0 bg-gradient-to-t from-black/80 via-transparent to-transparent" />
          <div className="absolute bottom-3 left-4 flex items-center gap-2 text-xs text-white/90">
            <span className="flex h-2 w-2 animate-pulse rounded-full bg-emerald-400" />
            {fs.regions.length} active region{fs.regions.length === 1 ? "" : "s"} on the global network
          </div>
        </div>
        <div className="flex flex-col divide-y divide-border">
          {Object.entries(catalog).map(([continent, regions]) => {
            const expanded = open[continent] ?? false;
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
                          <span>{r.label} - {r.aws} - {r.id}</span>
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
