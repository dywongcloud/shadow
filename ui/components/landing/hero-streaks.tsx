/** Two corner-anchored blue light streaks at the very top of the landing hero.
 *  The geometry lives in /landing/streak-left.webp and /landing/streak-right.webp,
 *  a one-time Chromium rasterization of our own re-authored SVG (the measured
 *  affine matrices, gradient stops and Gaussian falloff). The serving page runs
 *  no blur filter and ships no third-party bytes. Static decoration: no state,
 *  no animation, no pointer events, and no information carried by light alone.
 *
 *  Print and forced-colors drop background images, so each corner falls back to
 *  an SVG bracket tracing the two beam centerlines as 3px strokes (CanvasText /
 *  black). It is hidden in normal mode, where the beams themselves carry the
 *  form; a diagonal cue must not become a fourth luminous system. */
export function HeroStreaks() {
  return (
    <div className="lp-streaks" aria-hidden="true">
      <svg className="lp-streaks__marker lp-streaks__marker--left" viewBox="0 0 160 120" preserveAspectRatio="none" fill="none" focusable="false">
        <path d="M2 2 L148 106" stroke="currentColor" strokeWidth="3" />
        <path d="M24 2 L158 96" stroke="currentColor" strokeWidth="3" />
      </svg>
      <svg className="lp-streaks__marker lp-streaks__marker--right" viewBox="0 0 160 120" preserveAspectRatio="none" fill="none" focusable="false">
        <path d="M158 2 L12 106" stroke="currentColor" strokeWidth="3" />
        <path d="M136 2 L2 96" stroke="currentColor" strokeWidth="3" />
      </svg>
    </div>
  );
}
