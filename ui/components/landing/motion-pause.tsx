"use client";

import { useEffect, useState } from "react";

const CHANGED = "lp-motion-pause";
const PAUSED = "lp-paused";

/** Pauses or resumes all of the landing's decorative motion: `html.lp-paused` stops the CSS loops and the terminal's typing, and SMIL (which no CSS
 *  can stop) is paused from script. Every control reads the same state, so two buttons never disagree. */
export function setMotionPaused(paused: boolean) {
  document.documentElement.classList.toggle(PAUSED, paused);
  document.querySelectorAll<SVGSVGElement>(".lp svg").forEach((svg) => (paused ? svg.pauseAnimations() : svg.unpauseAnimations()));
  window.dispatchEvent(new Event(CHANGED));
}

/** Whether the motion is paused and a toggle for it. */
export function useMotionPause(): [boolean, () => void] {
  const [paused, setPaused] = useState(false);
  useEffect(() => {
    const read = () => setPaused(document.documentElement.classList.contains(PAUSED));
    window.addEventListener(CHANGED, read);
    return () => window.removeEventListener(CHANGED, read);
  }, []);
  return [paused, () => setMotionPaused(!document.documentElement.classList.contains(PAUSED))];
}

/** The second Tab stop on the landing (after "Skip to content"), shown only when focused, as a skip link is: a keyboard visitor reaches Pause before
 *  the navigation and the calls to action, not after them. It also keeps a standing request: with `prefers-reduced-motion: reduce` the SMIL
 *  timelines start paused (the CSS media query already stops the loops), and the state is cleared when the landing unmounts. */
export function PauseSkip() {
  const [paused, toggle] = useMotionPause();
  useEffect(() => {
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)");
    const apply = () => reduce.matches && setMotionPaused(true);
    apply();
    reduce.addEventListener("change", apply);
    return () => {
      reduce.removeEventListener("change", apply);
      document.documentElement.classList.remove(PAUSED);
    };
  }, []);
  return (
    <button
      type="button"
      onClick={toggle}
      className="sr-only focus:not-sr-only focus:fixed focus:left-4 focus:top-4 focus:z-[300] focus:inline-flex focus:min-h-[44px] focus:items-center focus:rounded-md focus:bg-white focus:px-4 focus:text-[14px] focus:text-black"
    >
      {paused ? "Play animations" : "Pause animations"}
    </button>
  );
}
