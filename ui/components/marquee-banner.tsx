/* ------------------------------------------------------------------ *
 * Landing-page marquee banner.
 *
 * A WHITE banner with a thin black outline whose exact silhouette — a tab on the
 * top edge (left-of-centre) and a tab on the bottom edge (right) — is rendered
 * from the source path (2048×69) as an inline SVG (fill white + non-scaling black
 * stroke). An infinitely-scrolling "OWN YOUR CLOUD. ·" ticker rides inside the
 * solid strip band (y 23–55 of the 69-tall viewBox) in black. It sits after the
 * hero globe with a little black space above for contrast, and generous space
 * below before the device showcase.
 * ------------------------------------------------------------------ */

const PHRASE = "Own Your Cloud";

// The banner silhouette (matches /public/banner-shape.svg).
const SHAPE =
  "M0 23H564C567 23 570 21 573 19C578 14 582 8 586 5C589 2 592 0 596 0H719C722 0 724 2 727 4C732 8 735 13 740 18C743 21 747 23 751 23H2048V55H1624C1620 55 1617 59 1613 65C1611 67 1610 68 1608 68H1543C1541 68 1539 66 1538 65C1534 61 1531 58 1527 55H0V23Z";

// The solid strip occupies y23–55 of the 69-unit-tall shape; the tabs extend
// above/below it. Position the text band over the strip only.
const STRIP_TOP = "33.33%"; // 23 / 69
const STRIP_HEIGHT = "46.38%"; // 32 / 69

/** One copy of the ticker. Two sit side-by-side so the -50% translate loops seamlessly.
 *  Each phrase is followed by a perfectly round, centred bold black bullet (a real
 *  circle, not a glyph, so it's symmetric at any size) → "OWN YOUR CLOUD • …". */
function Track({ "aria-hidden": ariaHidden }: { "aria-hidden"?: boolean }) {
  return (
    <div className="flex shrink-0 items-center" aria-hidden={ariaHidden}>
      {Array.from({ length: 12 }).map((_, i) => (
        <span key={i} className="flex items-center">
          <span
            className="whitespace-nowrap px-8 font-bold uppercase tracking-[0.22em] text-black"
            style={{ WebkitTextStroke: "0.7px #000" }}
          >
            {PHRASE}
          </span>
          <span aria-hidden className="inline-block shrink-0 rounded-full bg-black" style={{ width: "0.42em", height: "0.42em" }} />
        </span>
      ))}
    </div>
  );
}

export function MarqueeBanner() {
  return (
    <section aria-label="Own Your Cloud" className="relative z-10 w-full pt-4 pb-12 sm:pt-6 sm:pb-16">
      {/* The element keeps the shape's native aspect ratio so the tabs aren't distorted. */}
      <div className="relative w-full" style={{ aspectRatio: "2048 / 69" }}>
        {/* White banner with a clean thin black outline. */}
        <svg
          viewBox="0 0 2048 69"
          className="absolute inset-0 h-full w-full"
          preserveAspectRatio="none"
          aria-hidden="true"
        >
          <path d={SHAPE} fill="white" stroke="black" strokeWidth={1} vectorEffect="non-scaling-stroke" />
        </svg>

        {/* Scrolling ticker, confined to the solid strip band (full-width, no tabs).
            Bold black text + round bullets; sized to fill the strip without bleeding. */}
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
