import { GitMerge, GitPullRequest } from "lucide-react";

/** Illustrations for the feature grid. Sample names, no counts, no durations, no prices; each is hidden from assistive technology and the card text
 *  carries the meaning. */

export function PullRequests() {
  const rows = [
    { n: "#128", t: "Add pricing page", s: "Preview ready", tone: "text-emerald-300", kind: "ok" },
    { n: "#127", t: "Fix auth redirect", s: "Building", tone: "text-amber-300", kind: "run" },
    { n: "#126", t: "Update dependencies", s: "Merged", tone: "text-violet-300", kind: "merged" },
  ];
  return (
    <div className="flex h-full flex-col justify-center gap-2.5 p-4" aria-hidden="true">
      {rows.map((r) => (
        <div key={r.n} className="flex items-center gap-3 rounded-xl border border-white/[0.08] bg-white/[0.035] px-3.5 py-3">
          {r.kind === "merged" ? <GitMerge className="h-4 w-4 shrink-0 text-violet-300" /> : <GitPullRequest className="h-4 w-4 shrink-0 text-emerald-300" />}
          <span className="lp-mono text-[12px] text-[var(--lp-dim)]">{r.n}</span>
          <span className="min-w-0 flex-1 truncate text-[14px] text-zinc-100">{r.t}</span>
          <span className={`lp-chip ${r.tone}`}>
            {r.kind === "run" ? <span className="lp-spinner" /> : <span className="lp-dot" />}
            {r.s}
          </span>
        </div>
      ))}
    </div>
  );
}

/** The mesh keystone: the hero's planet in miniature (the same limb gradient and halo strokes, a dark body, meridians and parallels clipped to the dome),
 *  nine nodes, ten links and a few pulses. Sample topology: it prints no count of real nodes. */
const ARC = "M30 320C30 140 190 24 410 24C630 24 790 140 790 320";
const MESH_NODES: [number, number][] = [[150, 236], [262, 152], [352, 222], [410, 98], [506, 160], [588, 240], [672, 176], [330, 82], [486, 276]];
const MESH_EDGES = ["M150 236L262 152", "M262 152L352 222", "M262 152L330 82", "M330 82L410 98", "M410 98L506 160", "M506 160L588 240", "M506 160L672 176", "M352 222L486 276", "M352 222L410 98", "M588 240L672 176"];
const MERIDIANS = ["M410 24V320", "M410 24C330 80 282 170 272 320", "M410 24C490 80 538 170 548 320", "M410 24C262 72 184 182 152 320", "M410 24C558 72 636 182 668 320"];

export function MeshKeystone() {
  return (
    <svg viewBox="0 0 820 320" className="h-full w-full" preserveAspectRatio="xMidYMax slice" aria-hidden="true">
      <defs>
        <linearGradient id="lp-mk-limb" gradientUnits="userSpaceOnUse" x1="30" y1="0" x2="790" y2="0">
          <stop offset="0" stopColor="#8b5cf6" stopOpacity="0.6" />
          <stop offset="0.3" stopColor="#60a5fa" />
          <stop offset="0.5" stopColor="#e0f2fe" />
          <stop offset="0.7" stopColor="#38bdf8" />
          <stop offset="1" stopColor="#22d3ee" stopOpacity="0.6" />
        </linearGradient>
        <clipPath id="lp-mk-clip">
          <path d={`${ARC}V320H30Z`} />
        </clipPath>
      </defs>
      <path d={`${ARC}V320H30Z`} fill="#07090f" fillOpacity="0.92" />
      <g clipPath="url(#lp-mk-clip)" fill="none" stroke="rgba(255,255,255,.13)" strokeWidth="1.2">
        {[118, 190, 262].map((y) => (
          <path key={y} d={`M0 ${y}H820`} />
        ))}
        {MERIDIANS.map((d) => (
          <path key={d} d={d} />
        ))}
        {MESH_EDGES.map((d) => (
          <path key={d} d={d} stroke="rgba(147,197,253,.38)" />
        ))}
      </g>
      <g fill="none" stroke="url(#lp-mk-limb)" strokeLinecap="round">
        <path d={ARC} strokeWidth="34" strokeOpacity="0.05" />
        <path d={ARC} strokeWidth="18" strokeOpacity="0.09" />
        <path d={ARC} strokeWidth="9" strokeOpacity="0.2" />
        <path d={ARC} strokeWidth="2.6" strokeOpacity="0.95" />
      </g>
      {[0, 3, 5, 8].map((i) => (
        <circle key={i} r="3.4" fill="#60a5fa" className="lp-pulse">
          <animateMotion path={MESH_EDGES[i]} dur={`${2.4 + i * 0.3}s`} begin={`${i * 0.5}s`} repeatCount="indefinite" />
        </circle>
      ))}
      {MESH_NODES.map(([x, y], i) => (
        <g key={i} transform={`translate(${x - 11} ${y - 11})`}>
          <rect width="22" height="22" rx="5" fill="#000" stroke={i === 3 ? "#5eead4" : "#2f6bff"} strokeWidth="1.6" />
          <rect x="6" y="6" width="10" height="10" rx="2" fill="none" stroke="#e5e7eb" strokeWidth="1.3" />
        </g>
      ))}
    </svg>
  );
}

const BARS = [6, 6, 6, 34, 52, 30, 8, 6, 6, 6, 6, 6, 6, 44, 60, 38, 12, 6, 6, 6, 6, 6, 6, 6, 26, 48, 22, 6];

export function ScaleToZero() {
  return (
    <div className="flex h-full flex-col justify-end gap-3 p-5" aria-hidden="true">
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
    ["A", "example.com"],
    ["CNAME", "www.example.com"],
    ["TXT", "_hive-verify.example.com"],
  ];
  return (
    <div className="flex h-full flex-col justify-center gap-2.5 p-4" aria-hidden="true">
      {rows.map(([type, name]) => (
        <div key={name} className="lp-mono flex items-center gap-2.5 rounded-lg border border-white/[0.08] bg-white/[0.035] px-3 py-2.5 text-[12px]">
          <span className="w-[52px] shrink-0 text-[var(--lp-dim)]">{type}</span>
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
    <div className="flex h-full flex-col justify-center gap-2.5 p-4" aria-hidden="true">
      {rows.map((r) => (
        <div key={r.id} className="flex items-center gap-3 rounded-xl border border-white/[0.08] bg-white/[0.035] px-3.5 py-3">
          <span className="lp-dot text-emerald-300" />
          <span className="lp-mono flex-1 text-[12.5px] text-zinc-100">{r.id}</span>
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
    ["info", "Detected framework: Next.js"],
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
