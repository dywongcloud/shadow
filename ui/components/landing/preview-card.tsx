import { Lock } from "lucide-react";

/** A deployed site in a browser frame: an illustration of "every deployment gets its own HTTPS address". Decorative, so hidden from assistive
 *  technology; the sample address follows the `<project>.shadw.app` form the docs use. Its state chip follows the hero terminal (`data-deploy` on
 *  `.lp-dome`): Building while the transcript types, Ready once it says so, and Ready when nothing types. */
export function PreviewCard({ className = "" }: { className?: string }) {
  return (
    <div className={`lp-glass lp-float ${className}`} aria-hidden="true">
      <div className="lp-glass-bar">
        <i />
        <i />
        <i />
        <span className="lp-chip lp-mono ml-3 min-w-0 flex-1 justify-start gap-2 !text-[12px]">
          <Lock className="h-3 w-3 shrink-0 text-emerald-300" /> my-app.shadw.app
        </span>
      </div>
      <div className="p-4">
        <div
          className="h-[104px] rounded-lg"
          style={{ background: "linear-gradient(135deg, rgba(233, 42, 103,.3), rgba(168, 83, 186,.34) 55%, rgba(232,121,249,.3))" }}
        />
        <div className="mt-3.5 h-3 w-3/5 rounded bg-white/15" />
        <div className="mt-2 h-3 w-4/5 rounded bg-white/10" />
        <div className="mt-2 h-3 w-2/5 rounded bg-white/10" />
        <div className="mt-4 flex items-center justify-between">
          <span className="lp-chip lp-deploy-ready text-emerald-300">
            <span className="lp-dot" /> Ready
          </span>
          <span className="lp-chip lp-deploy-building text-amber-300">
            <span className="lp-spinner" /> Building
          </span>
          <span className="lp-mono text-[11px] uppercase tracking-[0.14em] text-[var(--lp-dim)]">Illustrative</span>
        </div>
      </div>
    </div>
  );
}
