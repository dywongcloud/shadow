"use client";

import { useEffect, useState } from "react";
import Image from "next/image";
import { GlowRing } from "./glow-ring";

/** Module scope on purpose: `<Landing>` can remount within one page load (an auth flip), and the intro must play once per page load; only
 *  the client effect sets it, so SSR always renders the overlay and hydration agrees. */
let introPlayed = false;

const INTRO_FADE_AT_MS = 1500;
const INTRO_DONE_AT_MS = 2200;
const SKIP_EVENTS = ["pointerdown", "keydown", "wheel", "touchstart"] as const;
const SLOW_EFFECTIVE_TYPES = ["slow-2g", "2g", "3g"];

function connectionKnownSlow(): boolean {
  const conn = (
    navigator as Navigator & {
      connection?: { effectiveType?: string; saveData?: boolean };
    }
  ).connection;
  return (
    conn?.saveData === true ||
    (typeof conn?.effectiveType === "string" && SLOW_EFFECTIVE_TYPES.includes(conn.effectiveType))
  );
}

/** Intro loading animation: a full-screen black overlay with the ghost logo inside the rotating GlowRing, ~2 s of spin then a fade; once per
 *  PAGE LOAD, skipped for prefers-reduced-motion and on known-slow connections, and rendered from first paint (SSR, inline styles only).
 *  The hero's entrance (`.lp-rise`) waits for the `lp-ready` class this component puts on <html> the moment the overlay lets go. */
export function IntroLoader() {
  const [phase, setPhase] = useState<"show" | "fade" | "done">(() => (introPlayed ? "done" : "show"));
  useEffect(() => {
    if (introPlayed) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- re-syncs against the module-level `introPlayed` flag, which can flip between this render and effect commit on a fast remount; the lazy initializer alone can't observe that
      setPhase("done");
      return;
    }
    introPlayed = true;
    if (connectionKnownSlow() || window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) {
      setPhase("done");
      return;
    }
    document.documentElement.style.overflow = "hidden";
    let t1 = setTimeout(() => setPhase("fade"), INTRO_FADE_AT_MS);
    let t2 = setTimeout(() => setPhase("done"), INTRO_DONE_AT_MS);
    const skip = () => {
      clearTimeout(t1);
      clearTimeout(t2);
      setPhase("fade");
      t2 = setTimeout(() => setPhase("done"), INTRO_DONE_AT_MS - INTRO_FADE_AT_MS);
    };
    for (const e of SKIP_EVENTS) window.addEventListener(e, skip, { once: true, passive: true });
    return () => {
      clearTimeout(t1);
      clearTimeout(t2);
      for (const e of SKIP_EVENTS) window.removeEventListener(e, skip);
      document.documentElement.style.overflow = "";
    };
  }, []);
  useEffect(() => {
    if (phase === "done") document.documentElement.style.overflow = "";
    if (phase !== "show") document.documentElement.classList.add("lp-ready");
  }, [phase]);
  useEffect(() => () => document.documentElement.classList.remove("lp-ready"), []);
  if (phase === "done") return null;
  const size = "min(30rem, 86vw)";
  return (
    <div
      style={{
        position: "absolute",
        top: 0,
        left: 0,
        right: 0,
        height: "100vh",
        zIndex: 100,
        background: "#000000",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        opacity: phase === "fade" ? 0 : 1,
        transition: "opacity 700ms ease",
        pointerEvents: phase === "fade" ? "none" : "auto",
      }}
    >
      <div
        style={{
          position: "relative",
          width: size,
          height: size,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
        }}
      >
        <GlowRing style={{ position: "absolute", inset: 0 }} />
        <Image
          src="/shadw-logo-dark.png"
          alt="shadw"
          width={274}
          height={91}
          priority
          style={{
            position: "relative",
            zIndex: 1,
            height: "2.4rem",
            width: "auto",
            animation: "intro-logo-pan 2.2s ease-in-out infinite",
          }}
        />
      </div>
    </div>
  );
}
