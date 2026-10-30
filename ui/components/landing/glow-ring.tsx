/** The rotating aurora glow ring, the original landing's light in the top left corner: a conic gradient masked into a soft ring and blurred, so
 *  spinning the element walks the visible arc through the palette. It is the page's one blur filter besides the sticky nav's backdrop — a blur is
 *  what makes a ring read as light rather than as a stroked circle, and the mask (not a border) is what keeps its edge soft.
 *  The colours are the Turbo Flow ramp (pink, purple, blue); the ring honours `prefers-reduced-motion` and the visitor's Pause through `.lp-ring`. */
export function GlowRing({
  className = "",
  duration = 2.2,
  style,
}: {
  className?: string;
  duration?: number;
  style?: React.CSSProperties;
}) {
  const ring = "radial-gradient(closest-side, transparent 48%, rgba(0,0,0,0.55) 62%, black 72%, rgba(0,0,0,0.55) 84%, transparent 96%)";
  return (
    <div
      aria-hidden
      className={`lp-ring pointer-events-none rounded-full ${className}`}
      style={{
        background:
          "conic-gradient(from -160deg, rgba(233,42,103,0.6), rgba(168,83,186,0.6) 33%, rgba(42,138,246,0.62) 66%, rgba(233,42,103,0.6))",
        WebkitMaskImage: ring,
        maskImage: ring,
        filter: "blur(min(42px,3.3vw))",
        animation: `glow-ring-spin ${duration}s linear infinite`,
        ...style,
      }}
    />
  );
}
