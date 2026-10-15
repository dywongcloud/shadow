import { Check, Lock } from "lucide-react";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";

/** Illustrations only: sample names, no counts, no durations. Each is hidden from assistive technology and tagged Sample; the card text carries the
 *  meaning. A well is a minimum height, not a height, so a reader's larger text grows it instead of clipping it (the graph's well is fixed: it scales
 *  with its box, not with text). The graph is drawn in a box of its own aspect (the container's `cqw`/`cqh` size it to fit), so its labels are HTML at 11 px at every card width and
 *  sit just above the `main` line and just below the branch, wherever those fall. A gradient on a zero-height path paints NOTHING in the default
 *  `objectBoundingBox` units, so the `main` line's gradient is in user space. */
const BRANCH_LABEL = "lp-mono absolute whitespace-nowrap text-[11px] tracking-[0.08em] text-[var(--lp-dim)]";

function CommitGraph() {
  return (
    <div className="flex min-h-0 flex-1 flex-col px-3 pb-3 pt-3" aria-hidden="true">
      <div className="min-h-0 flex-1 [container-type:size]">
        <div className="relative mx-auto" style={{ width: "min(100cqw, calc(100cqh * 320 / 130))", aspectRatio: "320 / 130", marginTop: "max(0px, calc((100cqh - 100cqw * 130 / 320) / 2))" }}>
          <svg viewBox="0 0 320 130" className="absolute inset-0 h-full w-full">
            <defs>
              <linearGradient id="lp-main" gradientUnits="userSpaceOnUse" x1="16" y1="36" x2="304" y2="36">
                <stop offset="0" stopColor="#e92a67" stopOpacity=".15" />
                <stop offset=".5" stopColor="#2a8af6" />
                <stop offset="1" stopColor="#a853ba" />
              </linearGradient>
            </defs>
            <path d="M16 36H304" stroke="url(#lp-main)" strokeWidth="2" fill="none" />
            <path d="M100 36C100 66 120 92 150 92H220C250 92 270 66 280 36" stroke="#a853ba" strokeOpacity=".7" strokeWidth="2" fill="none" />
            {[40, 100, 160, 220].map((x) => (
              <circle key={x} cx={x} cy="36" r="5" fill="#000000" stroke="#2a8af6" strokeWidth="2" />
            ))}
            {[170, 212].map((x) => (
              <circle key={x} cx={x} cy="92" r="5" fill="#000000" stroke="#a853ba" strokeWidth="2" />
            ))}
            <circle cx="280" cy="36" r="6" fill="#e92a67" />
            <circle cx="280" cy="36" r="6" fill="#e92a67" className="lp-pulse">
              <animate attributeName="r" values="6;16;6" dur="2.8s" repeatCount="indefinite" />
              <animate attributeName="opacity" values=".55;0;.55" dur="2.8s" repeatCount="indefinite" />
            </circle>
          </svg>
          <span className={BRANCH_LABEL} style={{ left: "4%", bottom: "calc(72.3% + 9px)" }}>
            main
          </span>
          <span className={BRANCH_LABEL} style={{ left: "58%", top: "calc(70.8% + 9px)", transform: "translateX(-50%)" }}>
            feature/pricing
          </span>
        </div>
      </div>
    </div>
  );
}

function BuildLines() {
  const rows = [
    ["Cloning repository", true],
    ["Detected Next.js", true],
    ["Uploading outputs…", false],
  ] as const;
  return (
    <div className="lp-mono flex flex-1 flex-col px-4 pb-3 pt-3 text-[12.5px] text-zinc-200" aria-hidden="true">
      <div className="flex min-h-0 flex-1 flex-col justify-center gap-3">
        {rows.map(([text, done]) => (
          <div key={text} className="flex items-center gap-2.5">
            {done ? <Check className="h-3.5 w-3.5 shrink-0 text-emerald-300" /> : <span className="lp-spinner shrink-0" />}
            <span className={done ? "" : "text-[var(--lp-mute)]"}>{text}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function LiveAddress() {
  return (
    <div className="flex flex-1 flex-col px-[14px] pb-3 pt-3" aria-hidden="true">
      <div className="flex min-h-0 flex-1 flex-col justify-center gap-4">
        <div className="lp-chip lp-mono w-full justify-start gap-2 !px-[11px] !py-[9px] !text-[12.5px]">
          <Lock className="h-[14px] w-[14px] shrink-0 text-emerald-300" /> my-app.shadw.app
        </div>
        <div className="flex items-center gap-2.5">
          <span className="lp-chip text-emerald-300">
            <span className="lp-dot" /> Ready
          </span>
          <span className="h-2.5 w-2/5 rounded bg-white/12" />
          <span className="h-2.5 w-1/4 rounded bg-white/8" />
        </div>
      </div>
    </div>
  );
}

const STEPS = [
  {
    n: "01",
    title: "Connect your code",
    body: "Push to Git, upload a zip or point at a container image. A push starts a build.",
    visual: <CommitGraph />,
    well: "h-[172px]",
  },
  {
    n: "02",
    title: "Build it",
    body: "A build runs on a machine that has the toolchain it needs. A failed build keeps its project and its logs, so you can fix it and redeploy.",
    visual: <BuildLines />,
    well: "min-h-[172px]",
  },
  {
    n: "03",
    title: "Go live",
    body: "Every deployment gets its own HTTPS address. Pull requests on a connected GitHub repository get previews, and an earlier deployment can be promoted to production.",
    visual: <LiveAddress />,
    well: "min-h-[172px]",
  },
];

export function HowItWorks() {
  return (
    <Section className="pt-28 sm:pt-36" wash="violet">
      <SectionHead eyebrow="How it works" title="No pipeline to assemble." lead="Connect a repository and the platform builds it, gives it an address and lets you roll back to an earlier deployment." />
      <div className="relative mt-14">
        <div aria-hidden="true" className="lp-rail hidden md:block" />
        <div aria-hidden="true" className="lp-rail-v md:hidden" />
        <ol className="grid gap-5 md:grid-cols-3">
          {STEPS.map((s, i) => (
            <li key={s.n} className="lp-card lp-reveal flex flex-col" style={delay(i * 110)}>
              <div className={`lp-well m-[12px] mb-0 flex flex-col overflow-hidden ${s.well}`}>{s.visual}</div>
              <div className="flex flex-1 flex-col p-6 pt-5">
                <span className="lp-gradient-text lp-mono w-fit text-[13px] tracking-[0.2em]">{s.n}</span>
                <h3 className="lp-card-title mt-2">{s.title}</h3>
                <p className="lp-card-copy mt-2.5">{s.body}</p>
              </div>
            </li>
          ))}
        </ol>
      </div>
    </Section>
  );
}
