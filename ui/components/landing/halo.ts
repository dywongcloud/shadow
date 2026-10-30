/** A glow without a blur filter: a stack of strokes along a limb, widest and faintest first, so the light falls off smoothly instead of in rings. `half` is the
 *  halo's reach in the drawing's own units, `peak` its opacity at the line, and `count` the layers: layer k carries the drop in the target profile
 *  `peak * (1 - d / half) ** 2.4` between its own distance and the next one in, so the stack adds up to that profile with steps below a few 8-bit levels
 *  (the earlier eight-stroke halo stepped by whole rings and showed as banding on a black canvas). */
export function haloLayers(half: number, peak: number, count: number): { width: number; opacity: number }[] {
  const at = (d: number) => peak * Math.max(0, 1 - d / half) ** 2.4;
  return Array.from({ length: count }, (_, i) => {
    const d = (half * (count - i)) / count;
    const inner = (half * (count - i - 1)) / count;
    return { width: Math.round(d * 2 * 100) / 100, opacity: Math.round((at(inner) - at(d)) * 10000) / 10000 };
  });
}
