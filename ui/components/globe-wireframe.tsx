"use client";

import { useEffect, useRef, useState } from "react";

/**
 * The hero globe wireframe — a SINGLE inlined SVG whose SIZE is owned entirely by
 * CSS (so it can never fall back to its intrinsic 2048px size and shift left).
 *
 * The blue tracer animation is ALWAYS on: the SMIL timeline runs from load, in
 * full color, with no interaction gating. The only JS here strips the SVG's
 * intrinsic width/height so CSS can own sizing.
 */
export function GlobeWireframe({ className }: { className?: string }) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const [markup, setMarkup] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    fetch("/globe-wireframe.svg")
      .then((r) => r.text())
      .then((t) => {
        if (!active) return;
        const cleaned = t
          // Strip the XML prolog + the root <svg>'s intrinsic width/height so it
          // can never render at its 2048px natural size; CSS owns sizing.
          .replace(/<\?xml[^>]*\?>/, "")
          .replace(/(<svg\b[^>]*?)\s+width="[^"]*"/, "$1")
          .replace(/(<svg\b[^>]*?)\s+height="[^"]*"/, "$1");
        setMarkup(cleaned);
      })
      .catch(() => {});
    return () => {
      active = false;
    };
  }, []);

  return (
    <div className={className}>
      {/* Fixed aspect-ratio box → the SVG's containing block is a definite size. */}
      <div className="relative w-full overflow-hidden" style={{ aspectRatio: "2048 / 762" }}>
        {markup && (
          <div
            ref={hostRef}
            aria-hidden="true"
            // CSS owns sizing (overrides the SVG's width/height attributes).
            className="pointer-events-none absolute inset-0 select-none [&>svg]:block [&>svg]:h-full [&>svg]:w-full"
            dangerouslySetInnerHTML={{ __html: markup }}
          />
        )}
      </div>
    </div>
  );
}
