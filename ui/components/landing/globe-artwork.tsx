import { GLOBE_STROKE, globeSvgMarkup } from "@/components/globe-art";

/** "Own your cloud" keystone globe: the SAME /new artwork the share card and
 *  the /new empty state draw -- seven wireframe strokes, nine blue node
 *  squares, twelve links and a blue-white lit horizon. The markup is a module
 *  constant, never user input: no SMIL, no counts, no regions; the card text
 *  carries the meaning. `idPrefix` keeps this instance's gradient ids apart
 *  from the share card's. `lp-globe-art` lets print and forced-colors repaint
 *  the literal strokes. */
export function GlobeArtwork({ className, idPrefix }: { className?: string; idPrefix: string }) {
  return (
    <div
      className={`lp-globe-art ${className ?? ""}`}
      aria-hidden="true"
      dangerouslySetInnerHTML={{ __html: globeSvgMarkup({ stroke: GLOBE_STROKE, idPrefix, fillContainer: true }) }}
    />
  );
}
