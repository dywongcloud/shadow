"use client";

import { Search, Plug, X } from "lucide-react";
import { Input, Button } from "@/components/ui";
import { cn } from "@/lib/utils";

/**
 * Command bar for the /regions map — adapted from the reference design's
 * search box + country pills + "Connect" button, onto real functionality:
 * the search box highlights a real node/browser-peer on the map by name or
 * id (see `matchesQuery` in app/regions/page.tsx), the pills filter by real
 * `NodeInfo.region` values (never fake country data), and "Connect a node"
 * scrolls to + flashes the page's real join-a-node instructions (there's no
 * in-browser "connect" action this dashboard can perform on the visitor's
 * behalf).
 */
export interface RegionPill {
  value: string;
  label: string;
}

export function RegionsCommandBar({
  query,
  onQueryChange,
  regions,
  activeRegion,
  onRegionSelect,
  onConnectClick,
}: {
  query: string;
  onQueryChange: (v: string) => void;
  regions: RegionPill[];
  activeRegion: string | null;
  onRegionSelect: (region: string | null) => void;
  onConnectClick: () => void;
}) {
  return (
    <div className="flex flex-col gap-3 border-b border-border bg-card/60 px-4 py-3 sm:flex-row sm:items-center sm:justify-between">
      <div className="flex flex-1 flex-wrap items-center gap-2">
        <div className="relative w-full max-w-xs">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted" />
          <Input
            value={query}
            onChange={(e) => onQueryChange(e.target.value)}
            placeholder="Search a node or peer ID"
            className="pl-8"
          />
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          {regions.map((r) => (
            <button
              key={r.value}
              type="button"
              onClick={() => onRegionSelect(activeRegion === r.value ? null : r.value)}
              className={cn(
                "rounded-full border px-2.5 py-1 text-xs font-medium transition-colors",
                activeRegion === r.value
                  ? "border-accent bg-accent text-accent-fg"
                  : "border-border text-secondary hover:border-border-strong hover:text-fg",
              )}
            >
              {r.label}
            </button>
          ))}
          {activeRegion && (
            <button
              type="button"
              onClick={() => onRegionSelect(null)}
              className="inline-flex items-center gap-1 rounded-full px-2.5 py-1 text-xs font-medium text-muted hover:text-fg"
            >
              <X className="h-3 w-3" /> Clear
            </button>
          )}
        </div>
      </div>
      <Button variant="outline" onClick={onConnectClick} className="shrink-0">
        <Plug className="h-3.5 w-3.5" /> Connect a node
      </Button>
    </div>
  );
}
