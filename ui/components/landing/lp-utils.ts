"use client";

import { useEffect, type RefObject } from "react";

/** A per-element animation delay, read by `.lp-rise` and `.lp-reveal` through `var(--d)`. */
export const delay = (ms: number) => ({ "--d": `${ms}ms` }) as React.CSSProperties;

/** Marks every `.lp-reveal` inside `root` as seen the first time it scrolls into view (a one-shot observer; without IntersectionObserver
 *  everything is shown at once). Reduced motion and no-script are handled in the stylesheet. */
export function useReveal(root: RefObject<HTMLElement | null>) {
  useEffect(() => {
    const host = root.current;
    if (!host) return;
    const targets = Array.from(host.querySelectorAll<HTMLElement>(".lp-reveal"));
    if (!("IntersectionObserver" in window)) {
      for (const t of targets) t.dataset.in = "1";
      return;
    }
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (!e.isIntersecting) continue;
          (e.target as HTMLElement).dataset.in = "1";
          io.unobserve(e.target);
        }
      },
      { rootMargin: "0px 0px -8% 0px", threshold: 0.06 },
    );
    for (const t of targets) io.observe(t);
    return () => io.disconnect();
  }, [root]);
}

/** Moves the `--mx`/`--my` custom properties of every direct child of the pointer's container to the pointer, so each `.lp-card` can paint a
 *  spotlight under it. Throttled to one write per frame. */
export function trackSpotlight(container: HTMLElement) {
  let frame = 0;
  let x = 0;
  let y = 0;
  const paint = () => {
    frame = 0;
    for (const child of Array.from(container.children) as HTMLElement[]) {
      const r = child.getBoundingClientRect();
      child.style.setProperty("--mx", `${x - r.left}px`);
      child.style.setProperty("--my", `${y - r.top}px`);
    }
  };
  const onMove = (e: PointerEvent) => {
    x = e.clientX;
    y = e.clientY;
    if (!frame) frame = requestAnimationFrame(paint);
  };
  container.addEventListener("pointermove", onMove, { passive: true });
  return () => {
    container.removeEventListener("pointermove", onMove);
    if (frame) cancelAnimationFrame(frame);
  };
}
