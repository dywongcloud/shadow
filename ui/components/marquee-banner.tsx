
const PHRASE = "Own Your Cloud";

const SHAPE =
  "M0 23H564C567 23 570 21 573 19C578 14 582 8 586 5C589 2 592 0 596 0H719C722 0 724 2 727 4C732 8 735 13 740 18C743 21 747 23 751 23H2048V55H1624C1620 55 1617 59 1613 65C1611 67 1610 68 1608 68H1543C1541 68 1539 66 1538 65C1534 61 1531 58 1527 55H0V23Z";

const STRIP_TOP = "33.33%";
const STRIP_HEIGHT = "46.38%";

/** One copy of the ticker; two sit side by side so the -50% translate loops seamlessly. Each phrase is followed by a round, centred black bullet (a real
 *  circle, not a glyph, so it is symmetric at any size). */
function Track({ "aria-hidden": ariaHidden }: { "aria-hidden"?: boolean }) {
  return (
    <div className="flex shrink-0 items-center" aria-hidden={ariaHidden}>
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

export function MarqueeBanner() {
  return (
    <section aria-label="Own Your Cloud" className="relative z-10 w-full pt-4 pb-12 sm:pt-6 sm:pb-16">
      <div className="relative w-full" style={{ aspectRatio: "2048 / 69" }}>
        <svg
          viewBox="0 0 2048 69"
          className="absolute inset-0 h-full w-full"
          preserveAspectRatio="none"
          aria-hidden="true"
        >
          <path d={SHAPE} fill="#131316" stroke="rgba(255,255,255,0.92)" strokeWidth={1} vectorEffect="non-scaling-stroke" />
        </svg>

        <div
          className="absolute inset-x-0 overflow-hidden"
          style={{ top: STRIP_TOP, height: STRIP_HEIGHT, fontSize: "clamp(3px, 0.8vw, 12px)" }}
        >
          <div className="flex h-full w-max items-center animate-[marquee_30s_linear_infinite] motion-reduce:animate-none">
            <Track />
            <Track aria-hidden />
          </div>
        </div>
      </div>
    </section>
  );
}
