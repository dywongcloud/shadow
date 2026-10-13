"use client";

import { GLOBE_TRACERS_SVG } from "@/components/globe-tracers";

/** The hero globe: a raster globe image (WebP, PNG fallback) under an inline animated tracer overlay. Both halves are DECLARATIVE (no fetch, no effect,
 *  no JS) so the browser discovers the image while parsing the HTML instead of after hydration, and the tracers live in the document so their SMIL
 *  timeline animates and paints at once; the overlay is hidden for prefers-reduced-motion (SMIL cannot be paused from CSS). */
export function GlobeWireframe({ className }: { className?: string }) {
  return (
    <div className={className}>
      <div className="relative w-full overflow-hidden" style={{ aspectRatio: "2048 / 762" }}>
        <picture>
          <source
            type="image/webp"
            sizes="(max-width: 1200px) 100vw, 1152px"
            srcSet="/globe-wireframe-1024.webp 1024w, /globe-wireframe-1536.webp 1536w, /globe-wireframe.webp 2048w"
          />
          <img
            src="/globe-wireframe.png"
            alt=""
            aria-hidden="true"
            width={2048}
            height={762}
            fetchPriority="high"
            decoding="async"
            className="pointer-events-none absolute inset-0 block h-full w-full select-none"
          />
        </picture>
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-0 select-none motion-reduce:hidden [&>svg]:block [&>svg]:h-full [&>svg]:w-full"
          dangerouslySetInnerHTML={{ __html: GLOBE_TRACERS_SVG }}
        />
      </div>
    </div>
  );
}
