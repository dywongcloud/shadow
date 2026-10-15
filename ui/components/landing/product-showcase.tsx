import Image from "next/image";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";

/** Real screens: the dashboard's own `/` and a project's Deployments tab, captured at 2x (2880 x 1800) from a local shadw node whose projects were built by
 *  real deploys (no DOM edits). The only change to a frame is the hidden footer copyright; recapture whenever the dashboard changes. */
const NAT = { w: 2880, h: 1800 };
type Box = { x: number; y: number; w: number; h: number };

/** A window onto a capture: `box` is the region to show, in the capture's own pixels, so the interface type is shown at a size a visitor can read. The
 *  image element is wider than its window (the whole capture is scaled so `box` fills it), so `sizes` names the IMAGE's displayed width, not the window's. */
function Crop({ src, alt, box, sizes, className = "" }: { src: string; alt: string; box: Box; sizes: string; className?: string }) {
  return (
    <div className={`relative overflow-hidden ${className}`} style={{ aspectRatio: `${box.w} / ${box.h}` }}>
      <Image
        src={src}
        alt={alt}
        width={NAT.w}
        height={NAT.h}
        sizes={sizes}
        className="absolute max-w-none"
        style={{ width: `${(NAT.w / box.w) * 100}%`, left: `${(-box.x / box.w) * 100}%`, top: `${(-box.y / box.h) * 100}%` }}
      />
    </div>
  );
}

const PROJECTS = "/product/dashboard-projects.png";
const DEPLOYMENTS = "/product/dashboard-deployments.png";
const PROJECTS_ALT = "The shadw dashboard on a node with four sample projects, storefront, marketing-site, docs and status, each deployed, with recent previews and usage beside them.";

export function ProductShowcase() {
  return (
    <Section className="pt-28 sm:pt-36" wash="cyan">
      <SectionHead
        center
        eyebrow="The dashboard"
        title={
          <>
            Every project, build and preview in <span className="lp-gradient-text">one place.</span>
          </>
        }
        lead="Projects, deployments, logs, domains and usage, in the same dashboard you deploy from."
      />

      <div className="lp-reveal relative mx-auto mt-14 hidden w-full pb-14 lg:block" style={delay(100)}>
        <div
          aria-hidden="true"
          className="pointer-events-none absolute -inset-x-[10%] -inset-y-[14%] -z-10"
          style={{ background: "radial-gradient(60% 56% at 50% 46%, rgba(99,102,241,0.4), rgba(34,211,238,0.16) 56%, transparent 76%)" }}
        />
        <figure className="lp-glass lp-limb-top overflow-hidden">
          <div className="lp-glass-bar" aria-hidden="true">
            <i />
            <i />
            <i />
          </div>
          <Crop src={PROJECTS} alt={PROJECTS_ALT} box={{ x: 0, y: 0, w: 2880, h: 1050 }} sizes="(max-width: 1200px) 100vw, 1152px" />
        </figure>
        <figure className="lp-glass lp-limb-top lp-float absolute bottom-0 right-0 z-10 w-[58%] overflow-hidden">
          <Crop
            src={DEPLOYMENTS}
            alt="A project's Deployments tab: one production build and two previews, all ready, with Redeploy and Instant Rollback."
            box={{ x: 70, y: 216, w: 1330, h: 560 }}
            sizes="1330px"
          />
        </figure>
      </div>

      <figure className="lp-reveal lp-glass lp-limb-top mx-auto mt-12 max-w-[560px] overflow-hidden lg:hidden" style={delay(100)}>
        <Crop src={PROJECTS} alt={PROJECTS_ALT} box={{ x: 690, y: 360, w: 810, h: 340 }} sizes="(max-width: 600px) 1280px, 1990px" />
      </figure>

      <p className="lp-mono mt-8 text-center text-[12px] uppercase leading-[1.7] tracking-[0.16em] text-[var(--lp-dim)] lg:mt-6">
        Screens from a local shadw node with sample projects.
      </p>
    </Section>
  );
}
