"use client";

import { useEffect, useState } from "react";
import { COUNTRY_GEO } from "./country-geo";

// Region TV — the atlas "TV logic" from PeerSpeak/dial.wtf mapped onto THIS
// fleet's regions AND the whole world. Data source: iptv-org's public channel
// + stream catalog (https://github.com/iptv-org/iptv, MIT — the dataset that
// powers tv.garden). No API key; joined server-side by /api/tv/catalog and
// fetched once per session (never on page load).

export interface TvChannel {
  id: string;
  name: string;
  country: string;
  categories: string[];
  /** Playable stream URL (HLS .m3u8, https). */
  url: string;
}

export interface RegionTv {
  region: string;
  /** ISO country the region's channels come from. */
  country: string;
  lat: number;
  lon: number;
  channels: TvChannel[];
}

/** A country the catalog has playable channels for, placed at its map centroid.
 *  This is the whole-world view; `RegionTv` is the fleet-region subset. */
export interface CountryTv {
  country: string;
  name: string;
  flag: string;
  lat: number;
  lon: number;
  channels: TvChannel[];
}

/** What the player is pointed at — a country or a serving region, uniformly. */
export interface TvTarget {
  /** Display label (country or region name). */
  label: string;
  /** ISO country code — also safe as a Drive path segment for recordings. */
  country: string;
  channels: TvChannel[];
}

/** The fleet's serving regions → geo + the country whose national catalog
 *  represents them. Static by design (regions are operator-provisioned; a
 *  region missing here simply gets no TV plot — never an error). */
export const REGION_TV_GEO: Record<string, { lat: number; lon: number; country: string }> = {
  bangkok: { lat: 13.7563, lon: 100.5018, country: "TH" },
  "hong-kong": { lat: 22.3193, lon: 114.1694, country: "HK" },
  virginia: { lat: 39.0438, lon: -77.4874, country: "US" },
  "san-jose": { lat: 37.3382, lon: -121.8863, country: "US" },
  "sao-paulo": { lat: -23.5505, lon: -46.6333, country: "BR" },
  frankfurt: { lat: 50.1109, lon: 8.6821, country: "DE" },
};

/** ISO 3166-1 alpha-2 → its flag emoji (a regional-indicator pair), computed
 *  from the code so no flag data needs storing. */
export function flagEmoji(cc: string): string {
  const c = cc.trim().toUpperCase();
  if (!/^[A-Z]{2}$/.test(c)) return "🏳";
  return String.fromCodePoint(...[...c].map((ch) => 0x1f1e6 + ch.charCodeAt(0) - 65));
}

let catalogPromise: Promise<Map<string, TvChannel[]>> | null = null;

/** country code → playable channels. Fetched from THIS origin's proxy
 *  (`/api/tv/catalog`) — the browser can't reach iptv-org directly under the
 *  app's CSP, and the join runs server-side there. Cached once per session. */
export function loadTvCatalog(): Promise<Map<string, TvChannel[]>> {
  if (catalogPromise) return catalogPromise;
  catalogPromise = (async () => {
    const res = await fetch("/api/tv/catalog");
    if (!res.ok) throw new Error(`TV catalog fetch failed (${res.status})`);
    const data = (await res.json()) as {
      byCountry?: Record<string, TvChannel[]>;
      error?: string;
    };
    if (data.error) throw new Error(data.error);
    const byCountry = new Map<string, TvChannel[]>();
    for (const [country, list] of Object.entries(data.byCountry ?? {})) {
      byCountry.set(country, list);
    }
    return byCountry;
  })().catch((e) => {
    catalogPromise = null; // a transient failure must not poison the session
    throw e;
  });
  return catalogPromise;
}

/** Route a third-party HLS URL through this origin's proxy so hls.js's fetches
 *  are same-origin (CSP `connect-src 'self'`). */
export function proxiedStreamUrl(url: string): string {
  return `/api/tv/hls?url=${encodeURIComponent(url)}`;
}

/** The per-region view over the loaded catalog, for regions the mesh serves. */
export function regionTv(catalog: Map<string, TvChannel[]>, liveRegions: string[]): RegionTv[] {
  const seen = new Set<string>();
  const out: RegionTv[] = [];
  for (const region of liveRegions) {
    if (seen.has(region)) continue;
    seen.add(region);
    const geo = REGION_TV_GEO[region];
    if (!geo) continue;
    out.push({ region, ...geo, channels: catalog.get(geo.country) ?? [] });
  }
  return out;
}

/** Every country in the loaded catalog that has channels, richest first. A
 *  country with channels but no known centroid still appears (lat/lon NaN) so
 *  the searchable list can show it even when the map can't place it. */
export function worldTv(catalog: Map<string, TvChannel[]>): CountryTv[] {
  const out: CountryTv[] = [];
  for (const [country, channels] of catalog) {
    if (!channels.length) continue;
    const geo = COUNTRY_GEO[country];
    out.push({
      country,
      name: geo?.name ?? country,
      flag: flagEmoji(country),
      lat: geo?.lat ?? NaN,
      lon: geo?.lon ?? NaN,
      channels,
    });
  }
  out.sort((a, b) => b.channels.length - a.channels.length);
  return out;
}

/** Shared catalog hook — both the plots layer and the stats strip mount it;
 *  the module-level promise cache means exactly ONE fetch per session. */
export function useTvCatalog(): {
  catalog: Map<string, TvChannel[]> | null;
  error: string | null;
} {
  const [catalog, setCatalog] = useState<Map<string, TvChannel[]> | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let stop = false;
    loadTvCatalog()
      .then((c) => {
        if (!stop) setCatalog(c);
      })
      .catch((e) => {
        if (!stop) setError(String(e?.message ?? e));
      });
    return () => {
      stop = true;
    };
  }, []);
  return { catalog, error };
}
