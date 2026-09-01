"use client";

import { useEffect, useState } from "react";
import { useTheme } from "next-themes";
import Image from "next/image";

/** Vercel-style globe-outline empty state. Shows the dark globe in dark mode and
 *  the light globe in light mode. The SVG is a half-globe wireframe anchored to
 *  the bottom of its container. */
export function GlobeEmptyState({ title, desc }: { title: string; desc?: string }) {
  const { resolvedTheme } = useTheme();
  const [mounted, setMounted] = useState(false);
  // eslint-disable-next-line react-hooks/set-state-in-effect -- hydration-mismatch-avoidance mount flag: render neutral state until client mount, then reveal theme-dependent image
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
        <Image src={src} alt="" width={688} height={256} unoptimized className="h-auto w-full select-none opacity-90" />
      </div>
    </div>
  );
}
