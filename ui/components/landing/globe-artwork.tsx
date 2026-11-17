import { GLOBE_STROKE, globeSvgMarkup } from "@/components/globe-art";

/** The /new deployment globe, drawn from the ONE markup builder that also
 *  builds the social card (`app/opengraph-image.tsx`). `public/globe-dark.svg`
 *  is the source the seven strokes are transcribed from and still renders
 *  /new's own empty state. The markup is a module constant, never user input:
 *  it carries no SMIL, no nodes, no counts and no regions; the card text
 *  carries the meaning. `idPrefix` keeps two instances from sharing an id.
 *  `lp-globe-art` lets print and forced-colors repaint the literal stroke. */
export function GlobeArtwork({ className, idPrefix }: { className?: string; idPrefix: string }) {
  return (
    <div
      className={`lp-globe-art ${className ?? ""}`}
      aria-hidden="true"
      dangerouslySetInnerHTML={{ __html: globeSvgMarkup({ stroke: GLOBE_STROKE, idPrefix, fillContainer: true }) }}
    />
  );
}
