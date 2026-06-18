"use client";

import { useEffect, useState } from "react";
import { useTheme } from "next-themes";

/** Vercel-style globe-outline empty state. Shows the dark globe in dark mode and
 *  the light globe in light mode. The SVG is a half-globe wireframe anchored to
 *  the bottom of its container. */
export function GlobeEmptyState({ title, desc }: { title: string; desc?: string }) {
  const { resolvedTheme } = useTheme();
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  const src = mounted && resolvedTheme === "dark" ? "/globe-dark.svg" : "/globe-light.svg";

  return (
    <div className="relative overflow-hidden">
      {(title || desc) && (
        <div className="relative z-10 pt-2">
          {title ? <h2 className="text-2xl font-semibold tracking-tight text-fg">{title}</h2> : null}
          {desc ? <p className="mt-1.5 text-sm text-secondary">{desc}</p> : null}
        </div>
      )}
      <div className="pointer-events-none mt-6 flex justify-center">
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img src={src} alt="" className="w-full select-none opacity-90" />
      </div>
    </div>
  );
}
