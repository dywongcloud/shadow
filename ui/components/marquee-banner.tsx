"use client";

import { useState } from "react";

const PHRASE = "Own Your Cloud";

/** The notched outline is the brand's one signature shape: a straight strip reads as any ticker, the two notches (top left of centre, bottom
 *  right of centre) are what make this one shadw's. The path is the original artwork, drawn on a 2048 x 69 box and stretched to the band. */
const SHAPE =
  "M0 23H564C567 23 570 21 573 19C578 14 582 8 586 5C589 2 592 0 596 0H719C722 0 724 2 727 4C732 8 735 13 740 18C743 21 747 23 751 23H2048V55H1624C1620 55 1617 59 1613 65C1611 67 1610 68 1608 68H1543C1541 68 1539 66 1538 65C1534 61 1531 58 1527 55H0V23Z";

const STRIP_TOP = "33.33%";
const STRIP_HEIGHT = "46.38%";

/** One copy of the ticker; two sit side by side so the -50% translate loops seamlessly. Each phrase is followed by a round, centred bullet (a real
 *  circle, not a glyph, so it is symmetric at any size). The copies are decorative: the section names the content for assistive technology. */
function Track() {
  return (
    <div className="flex shrink-0 items-center" aria-hidden="true">
      {Array.from({ length: 12 }).map((_, i) => (
        <span key={i} className="flex items-center">
          <span
            className="whitespace-nowrap px-8 font-bold uppercase tracking-[0.22em] text-white"
            style={{ WebkitTextStroke: "0.7px #fff" }}
          >
            {PHRASE}
          </span>
          <span aria-hidden className="inline-block shrink-0 rounded-full bg-white" style={{ width: "0.42em", height: "0.42em" }} />
        </span>
      ))}
    </div>
  );
}

/** The brand ticker. Its height is fixed, not proportional to the width, so the text keeps a readable size on a phone. Its button pauses ALL the
 *  landing's decorative motion (`html.lp-paused` stops the CSS loops and the terminal's typing; SMIL is paused from script), and
 *  `prefers-reduced-motion` stops it outright. */
export function MarqueeBanner() {
  const [paused, setPaused] = useState(false);
  const togglePause = () => {
    const next = !paused;
    setPaused(next);
    document.documentElement.classList.toggle("lp-paused", next);
    document.querySelectorAll<SVGSVGElement>(".lp svg").forEach((svg) => (next ? svg.pauseAnimations() : svg.unpauseAnimations()));
  };
  return (
    <section aria-label="Own Your Cloud" className="relative z-10 w-full pt-4 sm:pt-6">
      <div className="relative h-[52px] w-full sm:h-[68px]">
        <svg viewBox="0 0 2048 69" className="absolute inset-0 h-full w-full" preserveAspectRatio="none" aria-hidden="true">
          <path d={SHAPE} fill="#131316" stroke="rgba(255,255,255,0.92)" strokeWidth={1} vectorEffect="non-scaling-stroke" />
        </svg>

        <div className="absolute inset-x-0 overflow-hidden" style={{ top: STRIP_TOP, height: STRIP_HEIGHT, fontSize: "clamp(11px, 0.9vw, 14px)" }}>
          <div
            className="flex h-full w-max items-center animate-[marquee_30s_linear_infinite] motion-reduce:animate-none"
            style={{ animationPlayState: paused ? "paused" : "running" }}
          >
            <Track />
            <Track />
          </div>
        </div>
      </div>
      <div className="flex justify-end px-3 lg:px-7">
        <button
          type="button"
          onClick={togglePause}
          className="inline-flex min-h-[44px] items-center px-3 text-[12px] text-zinc-300 underline-offset-4 transition-colors hover:text-white hover:underline"
        >
          {paused ? "Play animations" : "Pause animations"}
        </button>
      </div>
    </section>
  );
}
