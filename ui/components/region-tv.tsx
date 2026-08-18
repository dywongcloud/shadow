"use client";

// World TV overlay — the dial.wtf atlas TV logic on THIS dashboard's world
// map, now covering EVERY country iptv-org has a playable stream for, not just
// the fleet's serving regions. Each country with channels gets a flag marker at
// its geographic centroid; the serving regions keep the brighter green "OLED"
// glassmorphic label. Clicking any marker (or a row in the searchable list
// below the map) opens that place's live TV. Plots use the SAME static
// projection the choropleth paints with (projectGeo/MAP_VIEW), as percentage
// offsets over the SVG box — correct at any rendered size.

import { ChevronDown, ChevronUp, Circle, HardDrive, Square, Tv, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { MAP_VIEW, projectGeo } from "@/components/world-choropleth";
import {
  proxiedStreamUrl,
  regionTv,
  useTvCatalog,
  worldTv,
  type TvTarget,
} from "@/lib/tv";
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
  const [open, setOpen] = useState<TvTarget | null>(null);

  const regions = useMemo(
    () => (catalog ? regionTv(catalog, liveRegions) : []),
    [catalog, liveRegions],
  );
  // Every OTHER country (serving-region countries already have their bright
  // label) that has a placeable centroid — a small flag marker each.
  const world = useMemo(() => {
    if (!catalog) return [];
    const serving = new Set(regions.map((r) => r.country));
    return worldTv(catalog).filter(
      (c) => !serving.has(c.country) && Number.isFinite(c.lat) && Number.isFinite(c.lon),
    );
  }, [catalog, regions]);

  return (
    <>
      {/* World coverage: one flag per country at its centroid. Small + dim so
          the map stays readable; hover scales it up and reveals name + count. */}
      {world.map((c) => {
        const p = projectGeo(c.lon, c.lat);
        if (!p) return null;
        const [x, y] = p;
        return (
          <button
            key={c.country}
            onClick={() => setOpen({ label: c.name, country: c.country, channels: c.channels })}
            className="absolute z-10 -translate-x-1/2 -translate-y-1/2 text-[13px] leading-none opacity-70 transition-transform hover:z-30 hover:scale-[1.7] hover:opacity-100"
            style={{ left: `${(x / MAP_VIEW.width) * 100}%`, top: `${(y / MAP_VIEW.height) * 100}%` }}
            title={`${c.name} · ${c.channels.length} channel${c.channels.length === 1 ? "" : "s"}`}
          >
            {c.flag}
          </button>
        );
      })}
      {/* Serving-region plots: brighter OLED text, drawn above the flags. */}
      {regions.map((r) => {
        const p = projectGeo(r.lon, r.lat);
        if (!p) return null;
        const [x, y] = p;
        return (
          <button
            key={r.region}
            onClick={() => setOpen({ label: r.region, country: r.country, channels: r.channels })}
            className={`absolute z-20 -translate-x-1/2 -translate-y-1/2 px-1.5 py-1 ${GLASS} ${OLED} cursor-pointer transition-transform hover:z-30 hover:scale-110`}
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

      {open && <RegionTvViewer target={open} onClose={() => setOpen(null)} />}
    </>
  );
}

/** Collapsible statistics strip — mounted by the page BELOW the map card. A
 *  searchable directory of EVERY country with channels; a row opens that
 *  country's TV. Shares the layer's catalog through the module-cached hook. */
export function RegionTvStats() {
  const { catalog } = useTvCatalog();
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  const [watch, setWatch] = useState<TvTarget | null>(null);
  const world = useMemo(() => (catalog ? worldTv(catalog) : []), [catalog]);
  const loaded = !!catalog;
  const totalChannels = world.reduce((n, c) => n + c.channels.length, 0);
  const filtered = useMemo(() => {
    const s = q.trim().toLowerCase();
    if (!s) return world;
    return world.filter(
      (c) => c.name.toLowerCase().includes(s) || c.country.toLowerCase().includes(s),
    );
  }, [world, q]);

  return (
    <div className={`relative mt-2 ${GLASS} px-3 py-2`}>
      <button onClick={() => setOpen(!open)} className={`flex w-full items-center justify-between ${OLED}`}>
        <span>
          WORLD TV GRID · {world.length} COUNTR{world.length === 1 ? "Y" : "IES"} ·{" "}
          {loaded ? `${totalChannels} LIVE CHANNELS` : "SYNCING"}
        </span>
        {open ? <ChevronUp className="h-3.5 w-3.5" /> : <ChevronDown className="h-3.5 w-3.5" />}
      </button>
      {open && (
        <div className="mt-3">
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="SEARCH COUNTRY…"
            className={`mb-2 w-full ${GLASS} ${OLED} px-2 py-1 outline-none placeholder:text-green-400/40`}
          />
          <div className="grid max-h-64 grid-cols-2 gap-x-3 gap-y-1 overflow-y-auto sm:grid-cols-3">
            {filtered.map((c) => (
              <button
                key={c.country}
                onClick={() => setWatch({ label: c.name, country: c.country, channels: c.channels })}
                className={`flex items-center justify-between gap-1 ${OLED} hover:!text-green-200`}
                title={`Watch ${c.name}`}
              >
                <span className="flex items-center gap-1 truncate">
                  <span className="text-sm">{c.flag}</span>
                  <span className="truncate opacity-85">{c.name.toUpperCase()}</span>
                </span>
                <span className="shrink-0 opacity-70">{c.channels.length}</span>
              </button>
            ))}
            {!filtered.length && (
              <span className={`${OLED} opacity-60`}>{loaded ? "NO MATCH" : "SYNCING…"}</span>
            )}
          </div>
        </div>
      )}
      {watch && <RegionTvViewer target={watch} onClose={() => setWatch(null)} />}
    </div>
  );
}

/** The TV: full-viewport lightbox with the target's channels, hls.js playback
 *  with native-HLS fallback, fault-recovery, prev/next channel switching. */
function RegionTvViewer({ target, onClose }: { target: TvTarget; onClose: () => void }) {
  const [idx, setIdx] = useState(0);
  const [err, setErr] = useState<string | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const channel = target.channels[idx] ?? null;

  // Switching country/region resets to its first channel.
  useEffect(() => {
    setIdx(0);
  }, [target]);

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
      region: target.country, // ISO code — safe as the Drive path segment (tv/<cc>/…)
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
    if (!target.channels.length) return;
    setIdx((i) => (i + d + target.channels.length) % target.channels.length);
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/80 p-4 backdrop-blur-sm"
      onClick={onClose}
      role="dialog"
      aria-label={`Live TV — ${target.label}`}
    >
      <div className={`w-full max-w-3xl ${GLASS} p-3`} onClick={(e) => e.stopPropagation()}>
        <div className={`mb-2 flex items-center justify-between ${OLED}`}>
          <span>
            {target.label.toUpperCase()} [{target.country}] · CH {idx + 1}/{target.channels.length} ·{" "}
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
            <div className={`flex h-full items-center justify-center ${OLED}`}>NO CHANNELS HERE</div>
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
