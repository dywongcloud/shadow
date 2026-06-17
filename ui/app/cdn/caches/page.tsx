"use client";

import { useState } from "react";
import { Info, Tag, Image as ImageIcon, AlertTriangle, Check } from "lucide-react";
import { Button, Card } from "@/components/ui";
import { apiSend, usePoll } from "@/lib/api";
import { cn } from "@/lib/utils";

interface Cdn { hits: number; misses: number; stale: number; entries: number; hit_ratio: number }

type Scope = "tag" | "image" | "all";

export default function CachesPage() {
  const { data: cdn, refresh } = usePoll<Cdn>("/v1/cdn", 4000);
  const [tab, setTab] = useState<"purge" | "history">("purge");
  const [scope, setScope] = useState<Scope | "">("");
  const [tag, setTag] = useState("");
  const [done, setDone] = useState(false);
  const [busy, setBusy] = useState(false);

  async function purge() {
    if (!scope) return;
    setBusy(true);
    try {
      // The edge CDN purge clears cached entries; tag/image scopes are honored
      // best-effort against the same store.
      await apiSend("POST", "/v1/cdn/purge", { scope, tag });
      setDone(true);
      setTimeout(() => setDone(false), 2000);
      refresh();
    } catch (e) { alert(String(e)); } finally { setBusy(false); }
  }

  return (
    <div>
      <h1 className="text-2xl font-semibold tracking-tight">Caches</h1>
      <p className="mt-1 text-sm text-secondary">
        Hive caches pages, API responses, and static assets in regions close to users to improve performance.
      </p>

      <div className="mt-6 flex gap-1 border-b border-border">
        {(["purge", "history"] as const).map((t) => (
          <button key={t} onClick={() => setTab(t)}
            className={cn("-mb-px border-b-2 px-3 pb-2.5 pt-1 text-sm capitalize", tab === t ? "border-fg text-fg" : "border-transparent text-secondary hover:text-fg")}>
            {t === "purge" ? "Purge Cache" : "History"}
          </button>
        ))}
      </div>

      {/* CDN stats */}
      <div className="mt-6 grid grid-cols-2 gap-3 sm:grid-cols-4">
        <Stat label="Cache HITs" value={cdn?.hits ?? 0} />
        <Stat label="MISSes" value={cdn?.misses ?? 0} />
        <Stat label="Entries" value={cdn?.entries ?? 0} />
        <Stat label="Hit ratio" value={`${Math.round((cdn?.hit_ratio ?? 0) * 100)}%`} />
      </div>

      {tab === "purge" ? (
        <>
          <Card className="mt-6 flex items-center justify-between bg-subtle/60 py-3">
            <span className="flex items-center gap-2 text-sm text-secondary"><Info className="h-4 w-4" /> Hive's cache has multiple layers. Purge caches properly by first reviewing the cache order.</span>
            <Button variant="outline">Review Cache Order</Button>
          </Card>

          <Card className="mt-4">
            <h3 className="text-lg font-semibold">Purge cache</h3>
            <p className="mt-1 text-sm text-secondary">Programmatically purge Hive's caches to force fresh data on the next request.</p>
            <div className="mb-4 mt-5 text-sm font-medium">Which content do you want to purge?</div>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
              <Choice icon={<Tag className="h-4 w-4" />} title="Cache Tag" desc="Cached responses associated with a specific user-defined tag." active={scope === "tag"} onClick={() => setScope("tag")} />
              <Choice icon={<ImageIcon className="h-4 w-4" />} title="Source Image" desc="Image optimization transformed images based on original source image URL." active={scope === "image"} onClick={() => setScope("image")} />
              <Choice icon={<AlertTriangle className="h-4 w-4 text-amber-500" />} title="All content" desc="Purge all cached data and force revalidation across this entire project." active={scope === "all"} onClick={() => setScope("all")} />
            </div>
            {scope === "tag" && (
              <input value={tag} onChange={(e) => setTag(e.target.value)} placeholder="my-cache-tag"
                className="mt-4 w-full max-w-sm rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none" />
            )}
            <div className="mt-6 flex justify-end">
              <Button onClick={purge} disabled={!scope || busy}>{done ? <><Check className="h-4 w-4" /> Purged</> : "Purge"}</Button>
            </div>
          </Card>
        </>
      ) : (
        <Card className="mt-6 py-12 text-center text-sm text-secondary">No purge history yet.</Card>
      )}
    </div>
  );
}

function Stat({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <Card className="flex flex-col gap-1 p-4">
      <span className="text-[11px] uppercase tracking-wide text-muted">{label}</span>
      <span className="text-xl font-semibold tabular-nums">{value}</span>
    </Card>
  );
}

function Choice({ icon, title, desc, active, onClick }: { icon: React.ReactNode; title: string; desc: string; active: boolean; onClick: () => void }) {
  return (
    <button onClick={onClick} className={cn("flex flex-col items-start gap-2 rounded-lg border p-4 text-left transition-colors", active ? "border-fg bg-subtle" : "border-border hover:bg-subtle")}>
      <div className="flex w-full items-center justify-between">
        <span className="flex items-center gap-2 text-sm font-medium">{icon} {title}</span>
        <span className={cn("h-4 w-4 rounded-full border", active ? "border-fg bg-fg" : "border-border-strong")} />
      </div>
      <span className="text-xs text-secondary">{desc}</span>
    </button>
  );
}
