"use client";

import { X } from "lucide-react";
import { cn } from "@/lib/utils";

/**
 * Command bar for the /regions map — adapted from the reference design's
 * search box + country pills + "Connect" button, onto real functionality:
 * the search box highlights a real node/browser-peer on the map by name or
 * id (see `matchesQuery` in app/regions/page.tsx), the pills filter by real
 * `NodeInfo.region` values (never fake country data), and "Connect"
 * scrolls to + flashes the page's real join-a-node instructions (there's no
 * in-browser "connect" action this dashboard can perform on the visitor's
 * behalf). Visual style (underlined search, rectangular pills, a fully
 * rounded "Clear", a plain dark Connect button) matches the reference
 * screenshots pixel-for-pixel in dark mode; light mode mirrors the same
 * contrast weight since the reference itself is dark-only.
 */
export interface RegionPill {
  value: string;
  label: string;
}

export function RegionsCommandBar({
  query,
  onQueryChange,
  onSubmit,
  regions,
  activeRegion,
  onRegionSelect,
  onConnectClick,
}: {
  query: string;
  onQueryChange: (v: string) => void;
  /** Fired on Enter in the search field — reveals which countries have a
   *  real live node (green fill), matching the reference design's
   *  press-Enter-to-search behavior. */
  onSubmit?: () => void;
  regions: RegionPill[];
  activeRegion: string | null;
  onRegionSelect: (region: string | null) => void;
  onConnectClick: () => void;
}) {
  return (
    <div className="flex flex-col gap-3 border-b border-border bg-card/60 px-4 py-3">
      <input
        value={query}
        onChange={(e) => onQueryChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") onSubmit?.();
        }}
        placeholder="Enter a Shadow Node or Peer ID"
        className="w-full border-0 border-b border-border bg-transparent pb-2 text-sm text-fg placeholder:text-muted focus:border-border-strong focus:outline-none"
      />
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-1.5">
          {regions.map((r) => (
            <button
              key={r.value}
              type="button"
              onClick={() => onRegionSelect(activeRegion === r.value ? null : r.value)}
              className={cn(
                "rounded border px-2.5 py-1 text-xs font-medium transition-colors",
                activeRegion === r.value
                  ? "border-accent bg-accent text-accent-fg"
                  : "border-border-strong text-fg hover:bg-subtle",
              )}
            >
              {r.label}
            </button>
          ))}
          {activeRegion && (
            <button
              type="button"
              onClick={() => onRegionSelect(null)}
              className="inline-flex items-center gap-1 rounded-full border border-border-strong px-2.5 py-1 text-xs font-medium text-fg hover:bg-subtle"
            >
              <X className="h-3 w-3" /> Clear
            </button>
          )}
        </div>
        <button
          type="button"
          onClick={onConnectClick}
          className="shrink-0 rounded-md border border-[#202020] bg-[#202020] px-4 py-1.5 text-sm font-medium text-white hover:opacity-90 dark:border-[#202020] dark:bg-[#202020]"
        >
          Connect
        </button>
      </div>
    </div>
  );
}
