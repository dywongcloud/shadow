"use client";

import { useMemo, useRef, useState } from "react";
import { geoEqualEarth, geoPath, type GeoPermissibleObjects } from "d3-geo";
import { feature } from "topojson-client";
import type { Topology, GeometryCollection } from "topojson-specification";
import worldTopology from "world-atlas/countries-110m.json";
import { GPU_COLOR, type MapMarker, type SatelliteMarker } from "@/components/region-map";

/**
 * Real country-boundary choropleth (Natural Earth 110m via world-atlas),
 * projected with d3-geo — the /regions page's hero map. `RegionMap` (the
 * coarse dot-grid land texture) is left untouched: it's still used standalone
 * by the function region-picker (settings/functions), which has no
 * country-fill requirement.
 */

const WIDTH = 960;
const HEIGHT = 500;

interface CountryProps {
  name: string;
}
type CountryFeature = GeoJSON.Feature<GeoJSON.Geometry, CountryProps>;

const worldFeatures: CountryFeature[] = (
  feature(
    worldTopology as unknown as Topology,
    (worldTopology as unknown as Topology).objects.countries as GeometryCollection,
  ) as unknown as GeoJSON.FeatureCollection<GeoJSON.Geometry, CountryProps>
).features;

const projection = geoEqualEarth().fitSize(
  [WIDTH, HEIGHT],
  { type: "FeatureCollection", features: worldFeatures } as GeoJSON.FeatureCollection,
);
const pathGen = geoPath(projection);

/** Exported for overlay layers (the region TV plots) so they position with
 *  THE SAME static projection the map paints with — percentage offsets over
 *  the SVG's viewBox stay aligned at any rendered size. */
export function projectGeo(lon: number, lat: number): [number, number] | null {
  return projection([lon, lat]);
}
export const MAP_VIEW = { width: WIDTH, height: HEIGHT };

function project(lon: number, lat: number): [number, number] | null {
  return projection([lon, lat]);
}

// Each country's rings, projected ONCE at module load (projection is static)
// rather than per-marker-per-render — point-in-country tests below just read
// these, they never re-project.
interface ProjectedCountry {
  name: string;
  polygons: Array<Array<[number, number]>[]>; // polygon[] of ring[] of [x,y]
}
const projectedCountries: ProjectedCountry[] = worldFeatures.map((f) => {
  const g = f.geometry;
  const rawPolygons: number[][][][] =
    g.type === "Polygon"
      ? [g.coordinates as unknown as number[][][]]
      : g.type === "MultiPolygon"
        ? (g.coordinates as unknown as number[][][][])
        : [];
  const polygons = rawPolygons.map((rings) =>
    rings.map((ring) => ring.map(([lon, lat]) => project(lon, lat)).filter((p): p is [number, number] => p !== null)),
  );
  return { name: f.properties.name, polygons };
});

// Even-odd ray cast over an already-PROJECTED ring (x/y, not lon/lat).
function ringContains(x: number, y: number, ring: [number, number][]): boolean {
  let hit = false;
  for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) {
    const [xi, yi] = ring[i];
    const [xj, yj] = ring[j];
    if (yi > y !== yj > y && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi) hit = !hit;
  }
  return hit;
}

function polygonContains(x: number, y: number, rings: [number, number][][]): boolean {
  if (!rings.length || !ringContains(x, y, rings[0])) return false;
  for (let i = 1; i < rings.length; i++) {
    if (ringContains(x, y, rings[i])) return false; // inside a hole
  }
  return true;
}

function countryAt(x: number, y: number): string | null {
  for (const c of projectedCountries) {
    for (const poly of c.polygons) {
      if (polygonContains(x, y, poly)) return c.name;
    }
  }
  return null;
}

// Pre-computed per-feature screen-space bounds (d3-geo's own path.bounds),
// keyed by name, for the double-click zoom below — one geoPath.bounds() call
// per country at module load rather than per double-click.
const countryBounds = new Map<string, [[number, number], [number, number]]>();
for (const f of worldFeatures) {
  countryBounds.set(f.properties.name, pathGen.bounds(f as GeoPermissibleObjects));
}

const ZOOM_PADDING = 32;
const MAX_ZOOM_SCALE = 10;
const ZOOM_TRANSITION_MS = 500;

function zoomTransformFor(name: string | null): { tx: number; ty: number; scale: number } {
  const bounds = name ? countryBounds.get(name) : null;
  if (!bounds) return { tx: 0, ty: 0, scale: 1 };
  const [[x0, y0], [x1, y1]] = bounds;
  const w = x1 - x0;
  const h = y1 - y0;
  if (!(w > 0) || !(h > 0)) return { tx: 0, ty: 0, scale: 1 };
  const scale = Math.min((WIDTH - ZOOM_PADDING * 2) / w, (HEIGHT - ZOOM_PADDING * 2) / h, MAX_ZOOM_SCALE);
  const cx = (x0 + x1) / 2;
  const cy = (y0 + y1) / 2;
  return { tx: WIDTH / 2 - scale * cx, ty: HEIGHT / 2 - scale * cy, scale };
}

export function WorldChoropleth({
  markers,
  satellites = [],
  highlightId,
  activated = false,
}: {
  markers: MapMarker[];
  satellites?: SatelliteMarker[];
  /** A marker/satellite id to draw an emphasis ring around — driven by the
   *  command bar's search box, so typing a node name/id highlights it here. */
  highlightId?: string | null;
  /** True once the command bar's search has been submitted (Enter) — flips
   *  countries with a live node from outline-only to a solid green fill,
   *  with a neutral gray baseline fill under every other country, matching
   *  the reference design's "press Enter to reveal" behavior. Before this,
   *  the map stays a plain outline (no country singled out). */
  activated?: boolean;
}) {
  const [hover, setHover] = useState<{ name: string; x: number; y: number } | null>(null);
  // The one country currently zoomed into, or null for the full-world view.
  const [zoomedCountry, setZoomedCountry] = useState<string | null>(null);
  const zoomTimerRef = useRef<number | null>(null);

  // Double-click a country to pan+zoom into it. Switching directly from one
  // zoomed country to another would COMPOUND the transform if applied
  // in-place (each click's zoom stacking on the last) — so a double-click on
  // a DIFFERENT country while already zoomed first resets to the full view,
  // then re-zooms into the new target once that reset transition finishes;
  // the transform itself is always recomputed fresh from the full-view
  // baseline (never multiplied against the previous one), so there is
  // structurally no way to end up "double zoomed". Double-clicking the
  // already-zoomed country zooms back out.
  function handleDoubleClick(name: string) {
    if (zoomTimerRef.current != null) {
      window.clearTimeout(zoomTimerRef.current);
      zoomTimerRef.current = null;
    }
    if (zoomedCountry === name) {
      setZoomedCountry(null);
      return;
    }
    if (zoomedCountry) {
      setZoomedCountry(null);
      zoomTimerRef.current = window.setTimeout(() => {
        setZoomedCountry(name);
        zoomTimerRef.current = null;
      }, ZOOM_TRANSITION_MS);
      return;
    }
    setZoomedCountry(name);
  }

  const placedMarkers = useMemo(
    () =>
      markers
        .map((m) => {
          const p = project(m.lon, m.lat);
          return p ? { ...m, x: p[0], y: p[1] } : null;
        })
        .filter((m): m is MapMarker & { x: number; y: number } => m !== null),
    [markers],
  );

  const placedSatellites = useMemo(
    () =>
      satellites
        .map((s) => {
          const p = project(s.lon, s.lat);
          return p ? { ...s, x: p[0], y: p[1] } : null;
        })
        .filter((s): s is SatelliteMarker & { x: number; y: number } => s !== null),
    [satellites],
  );

  // A country is "active" (solid fill) when a live marker's projected point
  // falls geometrically inside it — no name-matching against the node's own
  // `country` string, so it's correct regardless of how that's spelled. Also
  // gives the hover tooltip a real per-country node count for free.
  const nodesByCountry = useMemo(() => {
    const m = new Map<string, MapMarker[]>();
    for (const marker of placedMarkers) {
      const name = countryAt(marker.x, marker.y);
      if (!name) continue;
      (m.get(name) ?? m.set(name, []).get(name)!).push(marker);
    }
    return m;
  }, [placedMarkers]);

  const { tx, ty, scale } = zoomTransformFor(zoomedCountry);

  return (
    <div className="relative">
      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        className="block w-full bg-white dark:bg-black"
        role="img"
        aria-label={`World map — ${nodesByCountry.size} countries with a live autheo node`}
      >
        <g
          style={{
            transform: `translate(${tx}px, ${ty}px) scale(${scale})`,
            transitionProperty: "transform",
            transitionDuration: `${ZOOM_TRANSITION_MS}ms`,
            transitionTimingFunction: "ease",
          }}
        >
          {worldFeatures.map((f) => {
            const active = nodesByCountry.has(f.properties.name);
            const hovered = hover?.name === f.properties.name;
            const d = pathGen(f as GeoPermissibleObjects) ?? "";
            // Colors sampled pixel-exact from the reference design: the
            // static dark-mode screenshots gave stroke #808080 / hover
            // #b3b3b3 / tooltip #707070; the "press Enter" demo recording
            // gave the activated-state fills (#4aa853 green for a country
            // with a real node, #343638 gray baseline for every other
            // country) via the same histogram-sampling method, not
            // eyeballed. Light mode mirrors the same contrast weight since
            // both references are dark-mode only.
            const className = hovered
              ? "fill-[#4d4d4d] stroke-[#4d4d4d] dark:fill-[#b3b3b3] dark:stroke-[#b3b3b3]"
              : active
                ? activated
                  ? "fill-[#4aa853] stroke-[#4aa853]"
                  : "fill-black stroke-black dark:fill-white dark:stroke-white"
                : activated
                  ? "fill-[#343638] stroke-[#808080]"
                  : "fill-transparent stroke-[#808080]";
            return (
              <path
                key={f.id != null ? String(f.id) : f.properties.name}
                d={d}
                className={className}
                strokeWidth={0.6 / scale}
                // SVG shapes with an unpainted fill (fill-transparent, the
                // inactive-country case before Enter is pressed) don't
                // hit-test by default (`pointer-events: visiblePainted`), so
                // hover/double-click would only ever fire over a handful of
                // countries. `all` hit-tests the whole shape regardless of
                // paint.
                style={{ pointerEvents: "all" }}
                onMouseMove={(e) => {
                  const rect = e.currentTarget.ownerSVGElement!.getBoundingClientRect();
                  setHover({
                    name: f.properties.name,
                    x: ((e.clientX - rect.left) / rect.width) * 100,
                    y: ((e.clientY - rect.top) / rect.height) * 100,
                  });
                }}
                onMouseLeave={() => setHover(null)}
                onDoubleClick={() => handleDoubleClick(f.properties.name)}
              >
                <title>{f.properties.name}</title>
              </path>
            );
          })}

          {/* Live fleet markers, projected with the same real geography — GPU
              ring / relay / guardian badges preserved from RegionMap. */}
          {placedMarkers.map((m) => {
            const emphasized = highlightId != null && m.id === highlightId;
            return (
              <g key={m.id}>
                {emphasized && (
                  <circle cx={m.x} cy={m.y} r={9} fill="none" stroke={m.color ?? "#10b981"} strokeWidth={1.2}>
                    <animate attributeName="r" values="6;11;6" dur="1.6s" repeatCount="indefinite" />
                    <animate attributeName="opacity" values="1;0.2;1" dur="1.6s" repeatCount="indefinite" />
                  </circle>
                )}
                <circle cx={m.x} cy={m.y} r={5} fill={m.color ?? "#10b981"} opacity={0.25} className="animate-pulse" />
                {m.isGpu && (
                  <circle cx={m.x} cy={m.y} r={3.6} fill="none" stroke={GPU_COLOR} strokeWidth={0.5} strokeDasharray="1.2 1" />
                )}
                <circle cx={m.x} cy={m.y} r={2.2} fill={m.color ?? "#10b981"} stroke="#ffffff" strokeWidth={0.5} />
                <title>{m.label || m.id}</title>
                {m.hasRelay && (
                  <circle cx={m.x + 3} cy={m.y - 3} r={1.3} fill="#a855f7" stroke="#ffffff" strokeWidth={0.35}>
                    <title>Relay — {m.label || m.id}</title>
                  </circle>
                )}
                {m.hasGuardian && (
                  <circle cx={m.x - 3} cy={m.y - 3} r={1.3} fill="#f59e0b" stroke="#ffffff" strokeWidth={0.35}>
                    <title>GuardianDB — {m.label || m.id}</title>
                  </circle>
                )}
              </g>
            );
          })}

          {/* Browser-node satellites — same diamond glyph language as RegionMap. */}
          {placedSatellites.map((s) => {
            const emphasized = highlightId != null && s.id === highlightId;
            const color = s.state === "degraded" || s.state === "suspended" ? "#94a3b8" : "#38bdf8";
            return (
              <g key={`sat-${s.id}`} opacity={0.9}>
                {emphasized && (
                  <circle cx={s.x} cy={s.y} r={7} fill="none" stroke={color} strokeWidth={1}>
                    <animate attributeName="r" values="4;8;4" dur="1.6s" repeatCount="indefinite" />
                  </circle>
                )}
                <rect
                  x={s.x - 1.8}
                  y={s.y - 1.8}
                  width={3.6}
                  height={3.6}
                  fill={color}
                  stroke="#ffffff"
                  strokeWidth={0.4}
                  transform={`rotate(45 ${s.x} ${s.y})`}
                />
                <title>{(s.label || s.id) + " · browser node (low-trust)"}</title>
              </g>
            );
          })}
        </g>
      </svg>

      {/* Floating gray hover tooltip — the "Norway"/"India"/"Russia" label
          from the reference design, enriched with the real live node count. */}
      {hover && (
        <div
          // #707070 — sampled pixel-exact from the reference tooltip.
          className="pointer-events-none absolute z-10 -translate-x-1/2 -translate-y-full rounded-md bg-[#707070] px-2.5 py-1 text-xs font-medium text-white shadow-lg"
          style={{ left: `${hover.x}%`, top: `${hover.y}%`, marginTop: -8 }}
        >
          {hover.name}
          {nodesByCountry.has(hover.name) && (
            <span className="ml-1.5 text-slate-300">
              · {nodesByCountry.get(hover.name)!.length} node{nodesByCountry.get(hover.name)!.length === 1 ? "" : "s"}
            </span>
          )}
        </div>
      )}
    </div>
  );
}
