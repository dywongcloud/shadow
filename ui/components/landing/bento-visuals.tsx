import { GitMerge, GitPullRequest } from "lucide-react";

/** Illustrations for the feature grid. Sample names, no counts, no durations, no prices; each is hidden from assistive technology and the card text
 *  carries the meaning. */

export function PullRequests() {
  const rows = [
    { n: "#128", t: "Add pricing page", s: "Ready", tone: "text-emerald-300", kind: "ok" },
    { n: "#127", t: "Fix auth redirect", s: "Building", tone: "text-amber-300", kind: "run" },
    { n: "#126", t: "Update dependencies", s: "Merged", tone: "text-violet-300", kind: "merged" },
  ];
  return (
    <div className="flex h-full flex-col justify-center gap-2.5 p-4" aria-hidden="true">
      {rows.map((r) => (
        <div key={r.n} className="flex items-center gap-3 rounded-xl border border-white/[0.08] bg-white/[0.035] px-3.5 py-3">
          {r.kind === "merged" ? <GitMerge className="h-4 w-4 shrink-0 text-violet-300" /> : <GitPullRequest className="h-4 w-4 shrink-0 text-emerald-300" />}
          <span className="lp-mono hidden text-[12px] text-[var(--lp-dim)] sm:inline">{r.n}</span>
          <span className="min-w-0 flex-1 text-[14px] text-zinc-100">{r.t}</span>
          <span className={`lp-chip ${r.tone}`}>
            {r.kind === "run" ? <span className="lp-spinner" /> : <span className="lp-dot" />}
            {r.s}
          </span>
        </div>
      ))}
    </div>
  );
}

/** The dome keystone: the hero's planet in miniature (the same limb gradient and halo strokes, a dark body, meridians and parallels clipped to the dome),
 *  nine nodes, ten links and a few pulses. Drawn in the pixel space of the wide card (760 x 400, with 70 px of headroom so the arc can cross the card's top
 *  edge); `compact` fits the whole dome into a small well. Sample topology: it prints no count of real nodes. */
const ARC = "M20 400C20 150 180 -30 380 -30C580 -30 740 150 740 400";
const DOME_NODES: [number, number][] = [[140, 300], [250, 205], [335, 290], [395, 95], [505, 190], [590, 300], [675, 240], [305, 35], [465, 350]];
const DOME_EDGES = ["M140 300L250 205", "M250 205L335 290", "M250 205L305 35", "M305 35L395 95", "M395 95L505 190", "M505 190L590 300", "M505 190L675 240", "M335 290L465 350", "M335 290L395 95", "M590 300L675 240"];
const MERIDIANS = ["M380 -30V400", "M380 -30C300 40 250 200 240 400", "M380 -30C460 40 510 200 520 400", "M380 -30C230 30 130 210 100 400", "M380 -30C530 30 630 210 660 400"];

export function DomeKeystone({ compact = false }: { compact?: boolean }) {
  const id = compact ? "lp-mk-s" : "lp-mk-l";
  return (
    <svg viewBox="0 -70 760 470" className="h-full w-full overflow-visible" preserveAspectRatio={compact ? "xMidYMax meet" : "xMidYMax slice"} aria-hidden="true">
      <defs>
        <linearGradient id={`${id}-limb`} gradientUnits="userSpaceOnUse" x1="20" y1="0" x2="740" y2="0">
          <stop offset="0" stopColor="#8b5cf6" stopOpacity="0.7" />
          <stop offset="0.3" stopColor="#60a5fa" />
          <stop offset="0.5" stopColor="#e0f2fe" />
          <stop offset="0.7" stopColor="#38bdf8" />
          <stop offset="1" stopColor="#22d3ee" stopOpacity="0.7" />
        </linearGradient>
        <clipPath id={`${id}-clip`}>
          <path d={`${ARC}V400H20Z`} />
        </clipPath>
      </defs>
      <path d={`${ARC}V400H20Z`} fill="#07090f" fillOpacity="0.94" />
      <g clipPath={`url(#${id}-clip)`} fill="none" stroke="rgba(255,255,255,.13)" strokeWidth="1.2">
        {[150, 250, 340].map((y) => (
          <path key={y} d={`M0 ${y}H760`} />
        ))}
        {MERIDIANS.map((d) => (
          <path key={d} d={d} />
        ))}
        {DOME_EDGES.map((d) => (
          <path key={d} d={d} stroke="rgba(147,197,253,.4)" />
        ))}
      </g>
      <g fill="none" stroke={`url(#${id}-limb)`} strokeLinecap="round">
        <path d={ARC} strokeWidth="44" strokeOpacity="0.04" />
        <path d={ARC} strokeWidth="26" strokeOpacity="0.07" />
        <path d={ARC} strokeWidth="14" strokeOpacity="0.13" />
        <path d={ARC} strokeWidth="7" strokeOpacity="0.26" />
        <path d={ARC} strokeWidth="3" strokeOpacity="1" />
      </g>
      {[0, 3, 5, 8].map((i) => (
        <circle key={i} r="3.6" fill="#60a5fa" className="lp-pulse">
          <animateMotion path={DOME_EDGES[i]} dur={`${2.4 + i * 0.3}s`} begin={`${i * 0.5}s`} repeatCount="indefinite" />
        </circle>
      ))}
      {DOME_NODES.map(([x, y], i) => (
        <g key={i} transform={`translate(${x - 12} ${y - 12})`}>
          <rect width="24" height="24" rx="5.5" fill="#000" stroke={i === 3 ? "#5eead4" : "#2f6bff"} strokeWidth="1.7" />
          <rect x="6.5" y="6.5" width="11" height="11" rx="2" fill="none" stroke="#e5e7eb" strokeWidth="1.4" />
        </g>
      ))}
    </svg>
  );
}

const BARS = [6, 6, 6, 34, 52, 30, 8, 6, 6, 6, 6, 6, 6, 44, 60, 38, 12, 6, 6, 6, 6, 6, 6, 6, 26, 48, 22, 6];

export function ScaleToZero() {
  return (
    <div className="relative flex h-full flex-col justify-end gap-3 p-5" aria-hidden="true">
      <span className="lp-chip absolute right-3 top-3 !text-[11px] uppercase tracking-[0.12em] text-[var(--lp-dim)]">Sample</span>
      <div className="flex h-[72px] items-end gap-[5px]">
        {BARS.map((h, i) => (
          <span
            key={i}
            className="w-[6px] flex-1 rounded-full"
            style={{ height: h, background: h > 10 ? "linear-gradient(to top, #22d3ee, #818cf8)" : "rgba(255,255,255,.16)" }}
          />
        ))}
      </div>
      <div className="lp-mono flex justify-between text-[10.5px] uppercase tracking-[0.14em] text-[var(--lp-dim)]">
        <span>Idle</span>
        <span>Wakes on request</span>
      </div>
    </div>
  );
}

export function Dns() {
  const rows = [
    ["A", "@"],
    ["CNAME", "www"],
    ["TXT", "_hive-verify"],
  ];
  return (
    <div className="flex h-full flex-col justify-center gap-2.5 p-4" aria-hidden="true">
      {rows.map(([type, name]) => (
        <div key={name} className="lp-mono flex items-center gap-2.5 rounded-lg border border-white/[0.08] bg-white/[0.035] px-3 py-2.5 text-[12px]">
          <span className="w-[44px] shrink-0 text-[var(--lp-dim)]">{type}</span>
          <span className="min-w-0 flex-1 truncate text-zinc-100">{name}</span>
          <span className="text-emerald-300">✓</span>
        </div>
      ))}
    </div>
  );
}

const ENGINES = ["Postgres", "Redis", "SQLite", "Blob", "Queue", "Vector", "Pub/Sub", "Realtime"];

export function ManagedData() {
  return (
    <div className="flex h-full flex-wrap content-center gap-2.5 p-5" aria-hidden="true">
      {ENGINES.map((e, i) => (
        <span key={e} className={`lp-chip ${i < 2 ? "!border-indigo-400/50 !bg-indigo-400/10" : ""}`}>
          <span className="lp-dot" style={{ color: ["#38bdf8", "#f87171", "#5eead4", "#fbbf24", "#a78bfa", "#f472b6", "#34d399", "#22d3ee"][i] }} />
          {e}
        </span>
      ))}
    </div>
  );
}

export function Promote() {
  const rows = [
    { id: "dpl-3f9a21c07b", live: true },
    { id: "dpl-b7d10e44a2", live: false },
    { id: "dpl-91c3f0a85e", live: false },
  ];
  return (
    <div className="flex h-full flex-col justify-center gap-2.5 px-5 py-4" aria-hidden="true">
      {rows.map((r) => (
        <div key={r.id} className="flex h-10 items-center gap-3 rounded-xl border border-white/[0.08] bg-white/[0.035] px-3.5">
          <span className="lp-dot text-emerald-300 max-[359px]:hidden" />
          <span className="lp-mono flex-1 whitespace-nowrap text-[12.5px] text-zinc-100">{r.id}</span>
          {r.live ? (
            <span className="lp-chip !border-emerald-300/40 !bg-emerald-300/10 text-emerald-200">Production</span>
          ) : (
            <span className="lp-chip text-zinc-200">Promote</span>
          )}
        </div>
      ))}
    </div>
  );
}

export function LogsMetrics() {
  const lines = [
    ["info", "Build started"],
    ["info", "Detected Next.js"],
    ["info", "Uploading build outputs…"],
    ["ready", "Deployment ready"],
  ];
  return (
    <div className="relative flex h-full items-stretch gap-4 p-4" aria-hidden="true">
      <span className="lp-chip absolute right-3 top-3 !text-[11px] uppercase tracking-[0.12em] text-[var(--lp-dim)]">Sample</span>
      <div className="lp-mono flex min-w-0 flex-1 flex-col justify-center gap-2 text-[12px]">
        {lines.map(([lvl, text]) => (
          <div key={text} className="flex gap-2.5">
            <span className={lvl === "ready" ? "text-emerald-300" : "text-[var(--lp-dim)]"}>{lvl === "ready" ? "ready" : "info "}</span>
            <span className="truncate text-zinc-200">{text}</span>
          </div>
        ))}
      </div>
      <svg viewBox="0 0 160 90" className="hidden h-full w-[40%] shrink-0 self-end pt-9 sm:block" preserveAspectRatio="none">
        <defs>
          <linearGradient id="lp-area" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0" stopColor="#818cf8" stopOpacity=".5" />
            <stop offset="1" stopColor="#818cf8" stopOpacity="0.1" />
          </linearGradient>
        </defs>
        <path d="M0 70C14 66 20 50 34 52S56 72 70 56 92 22 108 30 130 58 144 40L160 24V90H0Z" fill="url(#lp-area)" />
        <path d="M0 89H160" stroke="rgba(255,255,255,.22)" strokeWidth="1" vectorEffect="non-scaling-stroke" />
        <path d="M0 70C14 66 20 50 34 52S56 72 70 56 92 22 108 30 130 58 144 40L160 24" fill="none" stroke="#a5b4fc" strokeWidth="1.8" vectorEffect="non-scaling-stroke" />
      </svg>
    </div>
  );
}
