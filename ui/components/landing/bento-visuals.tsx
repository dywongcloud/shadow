import { GitMerge, GitPullRequest } from "lucide-react";

/** Illustrations for the feature grid. Sample names, no counts, no durations, no prices; each is hidden from assistive technology and the card text
 *  carries the meaning. */

export function PullRequests() {
  const rows = [
    { n: "#128", t: "Add pricing page", s: "Ready", tone: "text-emerald-300", kind: "ok" },
    { n: "#127", t: "Fix redirect", s: "Building", tone: "text-amber-300", kind: "run" },
    { n: "#126", t: "Bump packages", s: "Merged", tone: "text-[#d79be6]", kind: "merged" },
  ];
  return (
    <div className="flex flex-1 flex-col px-4 pb-4 pt-3" aria-hidden="true">
      <div className="flex min-h-0 flex-1 flex-col justify-center gap-2.5">
        {rows.map((r) => (
          <div key={r.n} className="flex items-center gap-3 rounded-xl border border-white/[0.08] bg-white/[0.035] px-3.5 py-3">
            {r.kind === "merged" ? <GitMerge className="h-4 w-4 shrink-0 text-[#d79be6] max-[359px]:hidden" /> : <GitPullRequest className="h-4 w-4 shrink-0 text-emerald-300 max-[359px]:hidden" />}
            <span className="lp-mono hidden text-[12px] text-[var(--lp-dim)] sm:inline">{r.n}</span>
            <span className="min-w-0 flex-1 text-[14px] text-zinc-100">{r.t}</span>
            <span className={`lp-chip ${r.tone}`}>
              {r.kind === "run" ? <span className="lp-spinner" /> : <span className="lp-dot" />}
              {r.s}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

const BARS = [6, 6, 6, 34, 52, 30, 8, 6, 6, 6, 6, 6, 6, 44, 60, 38, 12, 6, 6, 6, 6, 6, 6, 6, 26, 48, 22, 6];

export function ScaleToZero() {
  return (
    <div className="flex flex-1 flex-col px-5 pb-5 pt-3" aria-hidden="true">
      <div className="mt-auto flex flex-col gap-3">
        <div className="flex h-[72px] items-end gap-[5px]">
          {BARS.map((h, i) => (
            <span
              key={i}
              className="w-[6px] flex-1 rounded-full"
              style={{ height: h, background: h > 10 ? "linear-gradient(to top, #2a8af6, #a853ba)" : "rgba(255,255,255,.16)" }}
            />
          ))}
        </div>
        <div className="lp-mono flex justify-between text-[10.5px] uppercase tracking-[0.14em] text-[var(--lp-dim)]">
          <span>Idle</span>
          <span>Wakes on request</span>
        </div>
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
    <div className="flex flex-1 flex-col px-4 pb-4 pt-3" aria-hidden="true">
      <div className="flex min-h-0 flex-1 flex-col justify-center gap-2.5">
        {rows.map(([type, name]) => (
          <div key={name} className="lp-mono flex items-center gap-2.5 rounded-lg border border-white/[0.08] bg-white/[0.035] px-3 py-2.5 text-[12px]">
            <span className="w-[44px] shrink-0 text-[var(--lp-dim)]">{type}</span>
            <span className="min-w-0 flex-1 truncate text-zinc-100">{name}</span>
            <span className="text-emerald-300">✓</span>
          </div>
        ))}
      </div>
    </div>
  );
}

const ENGINES = ["Postgres", "Redis", "SQLite", "Blob", "Queue", "Vector", "Pub/Sub", "Realtime"];

export function ManagedData() {
  return (
    <div className="flex flex-1 flex-wrap content-center gap-2.5 p-5" aria-hidden="true">
      {ENGINES.map((e, i) => (
        <span key={e} className={`lp-chip ${i < 2 ? "!border-[#a853ba]/60 !bg-[#a853ba]/10" : ""}`}>
          <span className="lp-dot" style={{ color: ["#2a8af6", "#e92a67", "#a853ba", "#2a8af6", "#e92a67", "#a853ba", "#2a8af6", "#e92a67"][i] }} />
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
    <div className="flex flex-1 flex-col px-5 pb-4 pt-3" aria-hidden="true">
      <div className="flex min-h-0 flex-1 flex-col justify-center gap-2.5">
        {rows.map((r) => (
          <div key={r.id} className="flex min-h-10 flex-wrap items-center gap-x-2.5 gap-y-1.5 rounded-xl border border-white/[0.08] bg-white/[0.035] px-3 py-1">
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
    <div className="flex flex-1 flex-col px-4 pb-4 pt-3" aria-hidden="true">
      <div className="flex min-h-0 flex-1 items-stretch gap-4">
        <div className="lp-mono flex min-w-0 flex-1 flex-col justify-center gap-2 text-[12px]">
          {lines.map(([lvl, text]) => (
            <div key={text} className="flex gap-2.5">
              <span className={lvl === "ready" ? "text-emerald-300" : "text-[var(--lp-dim)]"}>{lvl === "ready" ? "ready" : "info "}</span>
              <span className="truncate text-zinc-200">{text}</span>
            </div>
          ))}
        </div>
        <svg viewBox="0 0 160 90" className="hidden h-[110px] w-[40%] shrink-0 self-end sm:block" preserveAspectRatio="none">
          <defs>
            <linearGradient id="lp-area" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stopColor="#a853ba" stopOpacity=".5" />
              <stop offset="1" stopColor="#a853ba" stopOpacity="0.1" />
            </linearGradient>
          </defs>
          <path d="M0 70C14 66 20 50 34 52S56 72 70 56 92 22 108 30 130 58 144 40L160 24V90H0Z" fill="url(#lp-area)" />
          <path d="M0 89H160" stroke="rgba(255,255,255,.22)" strokeWidth="1" vectorEffect="non-scaling-stroke" />
          <path d="M0 70C14 66 20 50 34 52S56 72 70 56 92 22 108 30 130 58 144 40L160 24" fill="none" stroke="#d79be6" strokeWidth="1.8" vectorEffect="non-scaling-stroke" />
        </svg>
      </div>
    </div>
  );
}
