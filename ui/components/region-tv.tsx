"use client";

// Region TV overlay — the dial.wtf atlas TV logic on THIS dashboard's world
// map. Plots are green "OLED" digital text hovering over each serving
// region: fully transparent fill with a glassmorphic edge (backdrop blur +
// hairline border), glow from text-shadow, never a solid panel. Clicking a
// plot opens that region's live TV (iptv-org catalog for the region's
// country, HLS playback). A collapsible statistics strip renders below the
// map. Alignment: plots use the SAME static projection the choropleth paints
// with (projectGeo/MAP_VIEW), expressed as percentage offsets over the SVG
// box — correct at any rendered size. Known limit, deliberate: the map's
// double-click country zoom transforms only the SVG, so plots hold their
// full-world positions until zoom-out.

import { ChevronDown, ChevronUp, Circle, HardDrive, Square, Tv, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { MAP_VIEW, projectGeo } from "@/components/world-choropleth";
import { proxiedStreamUrl, regionTv, useTvCatalog, type RegionTv } from "@/lib/tv";
import {
  fmtBytes,
  fmtClock,
  pickMime,
  startRecording,
  type DvrRecording,
  type RecordHandle,
} from "@/lib/dvr";
import { usePoll, type Deployment } from "@/lib/api";

// Lazy hls.js, the StreamViewer pattern from the atlas: native HLS on
// Safari/iOS, hls.js everywhere else, loaded only in the browser on demand.
let HlsModule: typeof import("hls.js").default | null = null;
let hlsLoad: Promise<void> | null = null;
function loadHls(): Promise<void> {
  if (typeof window === "undefined") return Promise.resolve();
  if (HlsModule) return Promise.resolve();
  if (!hlsLoad) {
    hlsLoad = import("hls.js").then((m) => {
      HlsModule = m.default;
    });
  }
  return hlsLoad;
}

const OLED =
  "font-mono text-[11px] leading-none text-green-400 [text-shadow:0_0_6px_rgba(74,222,128,0.9),0_0_18px_rgba(74,222,128,0.35)]";
const GLASS = "bg-transparent backdrop-blur-[2px] border border-green-400/25 rounded";

export function RegionTvLayer({ liveRegions }: { liveRegions: string[] }) {
  const { catalog, error } = useTvCatalog();
  const [open, setOpen] = useState<RegionTv | null>(null);

  const regions = useMemo(
    () => (catalog ? regionTv(catalog, liveRegions) : []),
    [catalog, liveRegions],
  );

  return (
    <>
      {/* Plots: absolutely positioned inside the map's relative wrapper. */}
      {regions.map((r) => {
        const p = projectGeo(r.lon, r.lat);
        if (!p) return null;
        const [x, y] = p;
        return (
          <button
            key={r.region}
            onClick={() => setOpen(r)}
            className={`absolute z-10 -translate-x-1/2 -translate-y-1/2 px-1.5 py-1 ${GLASS} ${OLED} cursor-pointer transition-transform hover:scale-110`}
            style={{ left: `${(x / MAP_VIEW.width) * 100}%`, top: `${(y / MAP_VIEW.height) * 100}%` }}
            title={`Watch live TV near ${r.region}`}
          >
            <span className="flex items-center gap-1">
              <Tv className="h-3 w-3" aria-hidden />
              {r.region.toUpperCase()}
              <span className="opacity-70">·{r.channels.length}</span>
            </span>
          </button>
        );
      })}
      {!catalog && !error && (
        <span className={`absolute right-2 top-2 z-10 px-1.5 py-1 ${GLASS} ${OLED} animate-pulse`}>
          TV GRID SYNC…
        </span>
      )}
      {error && (
        <span className={`absolute right-2 top-2 z-10 px-1.5 py-1 ${GLASS} ${OLED} !text-amber-400`}>
          TV OFFLINE
        </span>
      )}

      {open && <RegionTvViewer region={open} onClose={() => setOpen(null)} />}
    </>
  );
}

/** Collapsible statistics strip — mounted by the page BELOW the map card.
 *  Shares the layer's catalog through the module-cached hook (one fetch). */
export function RegionTvStats({ liveRegions }: { liveRegions: string[] }) {
  const { catalog } = useTvCatalog();
  const [open, setOpen] = useState(false);
  const regions = useMemo(
    () => (catalog ? regionTv(catalog, liveRegions) : []),
    [catalog, liveRegions],
  );
  const loaded = !!catalog;
  const total = regions.reduce((n, r) => n + r.channels.length, 0);
  const categories = useMemo(() => {
    const m = new Map<string, number>();
    for (const r of regions)
      for (const c of r.channels) for (const cat of c.categories) m.set(cat, (m.get(cat) ?? 0) + 1);
    return [...m.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8);
  }, [regions]);

  return (
    <div className={`relative mt-2 ${GLASS} px-3 py-2`}>
      <button onClick={() => setOpen(!open)} className={`flex w-full items-center justify-between ${OLED}`}>
        <span>
          TV GRID · {regions.length} REGION{regions.length === 1 ? "" : "S"} ·{" "}
          {loaded ? `${total} LIVE CHANNELS` : "SYNCING"}
        </span>
        {open ? <ChevronUp className="h-3.5 w-3.5" /> : <ChevronDown className="h-3.5 w-3.5" />}
      </button>
      {open && (
        <div className="mt-3 grid gap-3 sm:grid-cols-2">
          <div className="flex flex-col gap-1.5">
            {regions.map((r) => (
              <div key={r.region} className={`flex items-center justify-between ${OLED}`}>
                <span className="opacity-80">
                  {r.region.toUpperCase()} [{r.country}]
                </span>
                <span>{r.channels.length} CH</span>
              </div>
            ))}
            {!regions.length && <span className={`${OLED} opacity-60`}>NO REGION DATA YET</span>}
          </div>
          <div className="flex flex-col gap-1.5">
            {categories.map(([cat, n]) => (
              <div key={cat} className={`flex items-center justify-between ${OLED}`}>
                <span className="uppercase opacity-80">{cat}</span>
                <span>{n}</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/** The TV: full-viewport lightbox with the region's channels, hls.js playback
 *  with native-HLS fallback, prev/next channel switching. */
function RegionTvViewer({ region, onClose }: { region: RegionTv; onClose: () => void }) {
  const [idx, setIdx] = useState(0);
  const [err, setErr] = useState<string | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const channel = region.channels[idx] ?? null;

  // ---- Virtual DVR ----------------------------------------------------
  // Drive is per-PROJECT, so a recording needs a destination project. Same
  // convention the Drive page uses: the tenant's deployed projects.
  const { data: deps } = usePoll<Deployment[]>("/deployments", 15000);
  const projects = useMemo(
    () => Array.from(new Set((deps ?? []).map((d) => d.project))).sort(),
    [deps],
  );
  const [project, setProject] = useState<string>("");
  useEffect(() => {
    if (!project && projects.length) setProject(projects[0]);
  }, [projects, project]);
  const [mins, setMins] = useState(30);
  const [recs, setRecs] = useState<DvrRecording[]>([]);
  const handleRef = useRef<RecordHandle | null>(null);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [elapsed, setElapsed] = useState(0);
  const canRecord = !!pickMime();

  const patch = (id: string, p: Partial<DvrRecording>) =>
    setRecs((list) => list.map((r) => (r.id === id ? { ...r, ...p } : r)));

  useEffect(() => {
    if (!activeId) return;
    const t = window.setInterval(() => setElapsed((e) => e + 1000), 1000);
    return () => window.clearInterval(t);
  }, [activeId]);

  function record() {
    const video = videoRef.current;
    if (!video || !channel || !project) return;
    const rec: DvrRecording = {
      id: `${Date.now()}`,
      project,
      region: region.region,
      channel: channel.name,
      startAt: Date.now(),
      durationMs: mins * 60_000,
      state: "recording",
      bytes: 0,
    };
    setRecs((l) => [rec, ...l]);
    setActiveId(rec.id);
    setElapsed(0);
    handleRef.current = startRecording({
      video,
      rec,
      onUpdate: (p) => {
        patch(rec.id, p);
        if (p.state && p.state !== "recording") {
          setActiveId((cur) => (cur === rec.id ? null : cur));
        }
      },
    });
  }

  function stopRecord() {
    handleRef.current?.stop();
    handleRef.current = null;
  }

  // Never leave a recorder running when the TV closes.
  useEffect(() => () => handleRef.current?.stop(), []);

  useEffect(() => {
    setErr(null);
    const video = videoRef.current;
    if (!video || !channel) return;
    let cancelled = false;
    let hls: import("hls.js").default | null = null;
    // Same-origin proxy so hls.js's playlist+segment fetches pass the app's
    // CSP (`connect-src 'self'`); arbitrary IPTV hosts can never be allowlisted.
    const src = proxiedStreamUrl(channel.url);

    // Live IPTV is inherently flaky: the auth token embedded in the stream URL
    // expires, ad-stitchers emit discontinuities, CDNs blip, and a single
    // proxied segment can time out. Each of those surfaces to the player as a
    // FATAL error, and with no recovery every one is a permanent freeze — the
    // reported symptom. So recover in place: reload the source on network
    // errors (which also re-mints an expired token via the upstream redirect),
    // recoverMediaError on media errors, and run a stall watchdog that nudges a
    // wedged element back to the live edge. Give up only after bounded retries.
    let netRetries = 0;
    let mediaRetries = 0;
    let lastTime = -1;
    let stalledFor = 0;
    let watchdog = 0;
    let recoverTimer = 0;

    const reload = () => {
      if (cancelled) return;
      try {
        if (hls) {
          hls.stopLoad();
          hls.loadSource(src);
          hls.startLoad();
        } else {
          video.src = src;
        }
        video.play().catch(() => {});
      } catch {
        /* torn down mid-recover */
      }
    };

    const nudgeToLive = () => {
      if (cancelled) return;
      try {
        const live = hls?.liveSyncPosition ?? NaN;
        if (Number.isFinite(live) && (live as number) > video.currentTime) {
          video.currentTime = live as number;
        } else if (video.seekable.length) {
          const end = video.seekable.end(video.seekable.length - 1);
          if (end - video.currentTime > 6) video.currentTime = end - 1;
        }
      } catch {
        /* seeking not permitted yet */
      }
    };

    const startWatchdog = () => {
      watchdog = window.setInterval(() => {
        if (cancelled || video.paused || video.ended) {
          stalledFor = 0;
          return;
        }
        if (video.currentTime === lastTime) {
          // Wedged while it should be playing: nudge to the live edge first,
          // then escalate to a full reload if that didn't unstick it.
          stalledFor += 3;
          if (stalledFor === 6) nudgeToLive();
          else if (stalledFor >= 12) {
            stalledFor = 0;
            reload();
          }
        } else {
          stalledFor = 0;
          lastTime = video.currentTime;
        }
      }, 3000);
    };

    if (video.canPlayType("application/vnd.apple.mpegurl")) {
      // Native HLS (Safari/iOS): the engine recovers most stalls itself; an
      // error->reload plus the watchdog cover the cases where it wedges.
      video.src = src;
      video.play().catch(() => {});
      video.addEventListener("error", reload);
      startWatchdog();
    } else {
      loadHls().then(() => {
        if (cancelled || !HlsModule) return;
        if (!HlsModule.isSupported()) {
          setErr("HLS not supported in this browser");
          return;
        }
        hls = new HlsModule({
          enableWorker: true,
          backBufferLength: 60,
          fragLoadingTimeOut: 30000,
          fragLoadingMaxRetry: 6,
          manifestLoadingTimeOut: 20000,
          manifestLoadingMaxRetry: 4,
          levelLoadingMaxRetry: 6,
        });
        hls.loadSource(src);
        hls.attachMedia(video);
        hls.on(HlsModule.Events.FRAG_BUFFERED, () => {
          // Progressing again — forget past failures so a later blip gets its
          // full retry budget, and clear any stale error banner.
          netRetries = 0;
          mediaRetries = 0;
          setErr(null);
        });
        hls.on(HlsModule.Events.ERROR, (_e, data) => {
          if (!data.fatal || cancelled || !HlsModule || !hls) return;
          switch (data.type) {
            case HlsModule.ErrorTypes.NETWORK_ERROR:
              if (netRetries < 8) {
                window.clearTimeout(recoverTimer);
                recoverTimer = window.setTimeout(reload, Math.min(1000 * (netRetries + 1), 5000));
                netRetries += 1;
              } else {
                setErr("stream error: network");
              }
              break;
            case HlsModule.ErrorTypes.MEDIA_ERROR:
              if (mediaRetries < 3) {
                mediaRetries += 1;
                hls.recoverMediaError();
              } else {
                setErr("stream error: media");
              }
              break;
            default:
              setErr(`stream error: ${data.type}`);
              hls.destroy();
          }
        });
        video.play().catch(() => {});
        startWatchdog();
      });
    }
    return () => {
      cancelled = true;
      window.clearInterval(watchdog);
      window.clearTimeout(recoverTimer);
      video.removeEventListener("error", reload);
      hls?.destroy();
      video.removeAttribute("src");
      video.load();
    };
  }, [channel]);

  const step = (d: number) => {
    if (!region.channels.length) return;
    setIdx((i) => (i + d + region.channels.length) % region.channels.length);
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/80 p-4 backdrop-blur-sm"
      onClick={onClose}
      role="dialog"
      aria-label={`Live TV — ${region.region}`}
    >
      <div className={`w-full max-w-3xl ${GLASS} p-3`} onClick={(e) => e.stopPropagation()}>
        <div className={`mb-2 flex items-center justify-between ${OLED}`}>
          <span>
            {region.region.toUpperCase()} [{region.country}] · CH {idx + 1}/{region.channels.length} ·{" "}
            {channel?.name?.toUpperCase() ?? "NO SIGNAL"}
          </span>
          <button onClick={onClose} aria-label="Close">
            <X className="h-4 w-4" />
          </button>
        </div>
        {/* The screen itself stays a real black rectangle — it is a TV; the
            chrome around it is the transparent glass. */}
        <div className="relative aspect-video w-full overflow-hidden rounded bg-black">
          {channel ? (
            <video ref={videoRef} className="h-full w-full" controls playsInline muted autoPlay />
          ) : (
            <div className={`flex h-full items-center justify-center ${OLED}`}>NO CHANNELS FOR THIS REGION</div>
          )}
          {err && (
            <div className={`absolute inset-x-0 bottom-0 p-2 text-center ${OLED} !text-amber-400`}>
              {err.toUpperCase()} — TRY NEXT CHANNEL
            </div>
          )}
        </div>
        <div className={`mt-2 flex items-center justify-between ${OLED}`}>
          <button onClick={() => step(-1)} className={`px-2 py-1 ${GLASS} hover:scale-105`}>
            ◀ PREV
          </button>
          <span className="opacity-70">IPTV-ORG PUBLIC CATALOG · STREAMS ARE THIRD-PARTY</span>
          <button onClick={() => step(1)} className={`px-2 py-1 ${GLASS} hover:scale-105`}>
            NEXT ▶
          </button>
        </div>

        {/* ---- VIRTUAL DVR: the cable-box deck ---- */}
        <div className={`mt-2 ${GLASS} p-2`}>
          <div className={`mb-2 flex items-center gap-2 ${OLED}`}>
            <HardDrive className="h-3.5 w-3.5" />
            <span className="tracking-widest">VIRTUAL DVR</span>
            <span className="opacity-60">· RECORDS ON YOUR BANDWIDTH · SAVES TO DRIVE /tv</span>
          </div>
          {!canRecord ? (
            <div className={`${OLED} !text-amber-400`}>
              THIS BROWSER CANNOT RECORD (NO MEDIARECORDER)
            </div>
          ) : (
            <div className="flex flex-wrap items-center gap-2">
              <button
                onClick={activeId ? stopRecord : record}
                disabled={!channel || !project}
                className={`flex items-center gap-1.5 px-2 py-1 ${GLASS} ${OLED} ${
                  activeId ? "!text-red-400 !border-red-400/40" : ""
                } disabled:opacity-40`}
              >
                {activeId ? <Square className="h-3 w-3 fill-current" /> : <Circle className="h-3 w-3 fill-current" />}
                {activeId ? `STOP · ${fmtClock(elapsed)}` : "● REC"}
              </button>
              <label className={`flex items-center gap-1 ${OLED}`}>
                LEN
                <select
                  value={mins}
                  onChange={(e) => setMins(Number(e.target.value))}
                  disabled={!!activeId}
                  className={`bg-transparent ${OLED} outline-none`}
                >
                  {[5, 15, 30, 60, 120].map((m) => (
                    <option key={m} value={m} className="bg-black">
                      {m}M
                    </option>
                  ))}
                </select>
              </label>
              <label className={`flex items-center gap-1 ${OLED}`}>
                TO
                <select
                  value={project}
                  onChange={(e) => setProject(e.target.value)}
                  disabled={!!activeId}
                  className={`max-w-[10rem] bg-transparent ${OLED} outline-none`}
                >
                  {projects.length === 0 && <option value="" className="bg-black">NO PROJECT</option>}
                  {projects.map((p) => (
                    <option key={p} value={p} className="bg-black">
                      {p.toUpperCase()}
                    </option>
                  ))}
                </select>
              </label>
              {activeId && (
                <span className={`${OLED} !text-red-400 animate-pulse`}>
                  ● RECORDING — KEEP THIS TAB OPEN
                </span>
              )}
            </div>
          )}
          {recs.length > 0 && (
            <div className="mt-2 flex flex-col gap-1 border-t border-green-400/20 pt-2">
              {recs.slice(0, 5).map((r) => (
                <div key={r.id} className={`flex items-center justify-between gap-2 ${OLED}`}>
                  <span className="truncate opacity-80">
                    {r.channel.toUpperCase()} · {fmtBytes(r.bytes)}
                  </span>
                  <span
                    className={
                      r.state === "failed"
                        ? "!text-amber-400"
                        : r.state === "saved"
                          ? "!text-green-300"
                          : ""
                    }
                  >
                    {r.state === "saved"
                      ? `SAVED → ${r.path}`
                      : r.state === "failed"
                        ? `FAILED · ${(r.error ?? "").slice(0, 60).toUpperCase()}`
                        : r.state.toUpperCase()}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
