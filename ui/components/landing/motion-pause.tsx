"use client";

import { useEffect, useState } from "react";
import { Pause as PauseIcon, Play as PlayIcon } from "lucide-react";

const CHANGED = "lp-motion-pause";
const PAUSED = "lp-paused";
const REMEMBERED = "lp-motion-paused";

/** A visitor's own choice survives a reload (storage can be blocked: then it lasts for the page). */
function remembered(): boolean {
  try {
    return window.localStorage.getItem(REMEMBERED) === "1";
  } catch {
    return false;
  }
}
function remember(paused: boolean) {
  try {
    if (paused) window.localStorage.setItem(REMEMBERED, "1");
    else window.localStorage.removeItem(REMEMBERED);
  } catch {
    // blocked storage: the choice lasts for this page only
  }
}

/** Pauses or resumes all of the landing's decorative motion: `html.lp-paused` stops the CSS loops and the terminal's typing, and SMIL (which no CSS
 *  can stop) is paused from script. Every control reads the same state, so two buttons never disagree. */
export function setMotionPaused(paused: boolean, chosen = true) {
  if (chosen) remember(paused);
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
 *  timelines start paused (the CSS media query already stops the loops), a remembered Pause is honoured, and the state is cleared when the
 *  landing unmounts. */
export function PauseSkip() {
  const [paused, toggle] = useMotionPause();
  useEffect(() => {
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)");
    // the system's request and the visitor's remembered choice start the page paused; neither is stored as a new choice
    const apply = () => (reduce.matches || remembered()) && setMotionPaused(true, false);
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
      <span aria-hidden="true">{paused ? <PlayIcon className="h-4 w-4" /> : <PauseIcon className="h-4 w-4" />}</span>
      <span className="sr-only">{paused ? "Play animations" : "Pause animations"}</span>
    </button>
  );
}
