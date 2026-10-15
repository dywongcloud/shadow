import { Check, Lock } from "lucide-react";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";

/** Illustrations only: sample names, no counts, no durations. Each is hidden from assistive technology; the card text carries the meaning. */
function CommitGraph() {
  return (
    <svg viewBox="0 0 320 130" className="h-full w-full" aria-hidden="true">
      <defs>
        <linearGradient id="lp-main" x1="0" x2="1">
          <stop offset="0" stopColor="#5eead4" stopOpacity=".15" />
          <stop offset=".5" stopColor="#22d3ee" />
          <stop offset="1" stopColor="#818cf8" />
        </linearGradient>
      </defs>
      <path d="M16 36H304" stroke="url(#lp-main)" strokeWidth="2" fill="none" />
      <path d="M100 36C100 66 120 92 150 92H220C250 92 270 66 280 36" stroke="#818cf8" strokeOpacity=".7" strokeWidth="2" fill="none" />
      {[40, 100, 160, 220].map((x) => (
        <circle key={x} cx={x} cy="36" r="5" fill="#0c0d10" stroke="#22d3ee" strokeWidth="2" />
      ))}
      {[170, 212].map((x) => (
        <circle key={x} cx={x} cy="92" r="5" fill="#0c0d10" stroke="#818cf8" strokeWidth="2" />
      ))}
      <circle cx="280" cy="36" r="6" fill="#5eead4" />
      <circle cx="280" cy="36" r="6" fill="#5eead4" className="lp-pulse">
        <animate attributeName="r" values="6;16;6" dur="2.8s" repeatCount="indefinite" />
        <animate attributeName="opacity" values=".55;0;.55" dur="2.8s" repeatCount="indefinite" />
      </circle>
      <text x="16" y="22" className="lp-mono" fontSize="10" fill="#9a9aa6" letterSpacing="1">main</text>
      <text x="150" y="116" className="lp-mono" fontSize="10" fill="#9a9aa6" letterSpacing="1">feature/pricing</text>
    </svg>
  );
}

function BuildLines() {
  const rows = [
    ["Cloning repository", true],
    ["Detected Next.js", true],
    ["Uploading outputs…", false],
  ] as const;
  return (
    <div className="lp-mono flex h-full flex-col justify-center gap-3 px-4 text-[12.5px] text-zinc-200" aria-hidden="true">
      {rows.map(([text, done]) => (
        <div key={text} className="flex items-center gap-2.5">
          {done ? <Check className="h-3.5 w-3.5 shrink-0 text-emerald-300" /> : <span className="lp-spinner shrink-0" />}
          <span className={done ? "" : "text-[var(--lp-mute)]"}>{text}</span>
        </div>
      ))}
    </div>
  );
}

function LiveAddress() {
  return (
    <div className="flex h-full flex-col justify-center gap-4 px-4" aria-hidden="true">
      <div className="lp-chip lp-mono w-full justify-start gap-2 !px-3 !py-2.5 !text-[12.5px]">
        <Lock className="h-3.5 w-3.5 shrink-0 text-emerald-300" /> my-app.shadw.app
      </div>
      <div className="flex items-center gap-2.5">
        <span className="lp-chip text-emerald-300">
          <span className="lp-dot" /> Ready
        </span>
        <span className="h-2.5 w-2/5 rounded bg-white/12" />
        <span className="h-2.5 w-1/4 rounded bg-white/8" />
      </div>
    </div>
  );
}

const STEPS = [
  {
    n: "01",
    title: "Connect your code",
    body: "Push to Git, upload a zip or point at a container image. Each push starts a build.",
    visual: <CommitGraph />,
  },
  {
    n: "02",
    title: "Build on a node",
    body: "A build runs on a node that has the toolchain it needs. A failed build keeps its project and its logs, so you can fix it and redeploy.",
    visual: <BuildLines />,
  },
  {
    n: "03",
    title: "Serve from the mesh",
    body: "Every deployment gets its own HTTPS address. Pull requests get previews, and any earlier build can be promoted to production.",
    visual: <LiveAddress />,
  },
];

export function HowItWorks() {
  return (
    <Section className="pt-28 sm:pt-36" wash="violet">
      <SectionHead eyebrow="How it works" title="From a commit to a live URL." lead="No pipeline to assemble. Connect a repository and the platform builds it, gives it an address and keeps every build." />
      <div className="relative mt-14">
        <div aria-hidden="true" className="lp-rail hidden md:block" />
        <div aria-hidden="true" className="lp-rail-v md:hidden" />
        <ol className="grid gap-5 md:grid-cols-3">
          {STEPS.map((s, i) => (
            <li key={s.n} className="lp-card lp-reveal flex flex-col" style={delay(i * 110)}>
              <div className="lp-well m-3.5 mb-0 h-[172px] overflow-hidden">{s.visual}</div>
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
