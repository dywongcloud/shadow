"use client";

import { useEffect, useMemo, useState } from "react";
import { Search, FileCode, FileText, Zap, Server, Loader2 } from "lucide-react";
import { Card } from "@/components/ui";
import { apiGet } from "@/lib/api";

interface FnRes { name: string; runtime: string; region: string; memory_mib: number; max_duration_secs: number; edge: boolean }
interface Asset { path: string; size: number; type: string }
interface Resources { functions: FnRes[]; static_assets: Asset[]; total_static: number; kind: string }

function fmtSize(b: number) {
  if (b < 1024) return `${b} B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} kB`;
  return `${(b / 1024 / 1024).toFixed(2)} MB`;
}

export function DeploymentResources({ deploymentId }: { deploymentId?: string }) {
  const [data, setData] = useState<Resources | null>(null);
  const [loading, setLoading] = useState(true);
  const [q, setQ] = useState("");
  const [fnLimit, setFnLimit] = useState(8);
  const [assetLimit, setAssetLimit] = useState(10);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch effect reacting to a changing deploymentId prop, not an initial-mount-only value
    if (!deploymentId) { setLoading(false); return; }
    setLoading(true);
    apiGet<Resources>(`/v1/deployments/${encodeURIComponent(deploymentId)}/resources`)
      .then(setData)
      .catch(() => setData(null))
      .finally(() => setLoading(false));
  }, [deploymentId]);

  const needle = q.trim().toLowerCase();
  const fns = useMemo(() => (data?.functions ?? []).filter((f) => f.name.toLowerCase().includes(needle)), [data, needle]);
  const assets = useMemo(() => (data?.static_assets ?? []).filter((a) => a.path.toLowerCase().includes(needle)), [data, needle]);

  if (!deploymentId) return <Card className="p-8 text-center text-sm text-secondary">No production deployment yet.</Card>;
  if (loading) return <div className="flex h-40 items-center justify-center"><Loader2 className="h-5 w-5 animate-spin text-muted" /></div>;

  return (
    <div>
      <div className="relative mb-5">
        <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
        <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Filter by path, name, etc." className="w-full rounded-lg border border-border bg-card py-2.5 pl-9 pr-3 text-sm focus:border-border-strong focus:outline-none" />
      </div>

      {/* Functions */}
      <div className="mb-6">
        <div className="mb-2 flex items-center gap-2 text-sm font-semibold">Functions <span className="text-muted">{fns.length}</span></div>
        <Card className="p-0">
          {fns.length === 0 ? (
            <div className="px-4 py-8 text-center text-sm text-secondary">No serverless/edge functions — this is a static deployment.</div>
          ) : (
            <>
              {fns.slice(0, fnLimit).map((f) => (
                <div key={f.name} className="grid grid-cols-[1fr_auto_auto_auto_auto] items-center gap-3 border-b border-border px-4 py-2.5 text-sm last:border-0">
                  <span className="flex items-center gap-2 truncate font-mono text-xs">
                    {f.edge ? <Zap className="h-3.5 w-3.5 text-amber-500" /> : <Server className="h-3.5 w-3.5 text-muted" />}
                    /{f.name}
                  </span>
                  <span className="rounded bg-subtle px-1.5 py-0.5 text-[10px] uppercase text-secondary">{f.region}</span>
                  <span className="text-xs text-secondary">{f.runtime}</span>
                  <span className="text-xs tabular-nums text-secondary">{f.memory_mib} MB</span>
                  <span className="text-xs tabular-nums text-muted">{f.max_duration_secs}s</span>
                </div>
              ))}
              {fns.length > fnLimit && (
                <button onClick={() => setFnLimit((n) => n + 12)} className="w-full border-t border-border py-2.5 text-center text-sm text-secondary hover:bg-subtle hover:text-fg">Show More</button>
              )}
            </>
          )}
        </Card>
      </div>

      {/* Static Assets */}
      <div>
        <div className="mb-2 flex items-center gap-2 text-sm font-semibold">Static Assets <span className="text-muted">{data?.total_static ?? 0}</span></div>
        <Card className="p-0">
          {assets.length === 0 ? (
            <div className="px-4 py-8 text-center text-sm text-secondary">No static assets.</div>
          ) : (
            <>
              {assets.slice(0, assetLimit).map((a) => (
                <div key={a.path} className="flex items-center justify-between gap-3 border-b border-border px-4 py-2.5 text-sm last:border-0">
                  <span className="flex items-center gap-2 truncate font-mono text-xs">
                    {a.type === "HTML" ? <FileText className="h-3.5 w-3.5 text-muted" /> : <FileCode className="h-3.5 w-3.5 text-muted" />}
                    {a.path}
                  </span>
                  <span className="flex shrink-0 items-center gap-4">
                    <span className="text-xs text-secondary">{a.type}</span>
                    <span className="w-16 text-right text-xs tabular-nums text-muted">{fmtSize(a.size)}</span>
                  </span>
                </div>
              ))}
              {assets.length > assetLimit && (
                <button onClick={() => setAssetLimit((n) => n + 20)} className="w-full border-t border-border py-2.5 text-center text-sm text-secondary hover:bg-subtle hover:text-fg">Show More</button>
              )}
            </>
          )}
        </Card>
      </div>
    </div>
  );
}
