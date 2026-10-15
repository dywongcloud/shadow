"use client";

import { useEffect, useRef, useState } from "react";

type Seg = { t: string; c?: string };
type Line = { at: number; segs: Seg[]; typed?: boolean; gap?: boolean; hang?: boolean };

/** The transcript is the shape `shadw deploy --follow` prints (crates/shadw-cli/src/main.rs `deploy`/`follow_build`) around real build-log
 *  lines (crates/hive-cloud/src/git.rs); the repository, project and deployment id are samples and no duration is printed. */
const ID = "dpl-3f9a21c07b";
const LINES: Line[] = [
  { at: 0, typed: true, segs: [{ t: "$ ", c: "lp-prompt" }, { t: "shadw deploy https://github.com/you/my-app \\" }] },
  { at: 1750, typed: true, segs: [{ t: "    --project my-app --follow" }] },
  { at: 2900, hang: true, segs: [{ t: `Deploy started: build ${ID} (project my-app)` }] },
  { at: 3200, hang: true, segs: [{ t: `Stream logs:  shadw build logs ${ID} --follow`, c: "lp-dim" }] },
  { at: 3900, gap: true, hang: true, segs: [{ t: "Cloning github.com/you/my-app (Branch: main, Commit: HEAD)" }] },
  { at: 4600, hang: true, segs: [{ t: "Detected framework: Next.js — primitive: Hybrid, package manager: npm (NpmLock)" }] },
  { at: 5300, hang: true, segs: [{ t: "…", c: "lp-dim" }] },
  { at: 5900, hang: true, segs: [{ t: "Uploading build outputs…" }] },
  { at: 6700, gap: true, hang: true, segs: [{ t: `Build ${ID} → ` }, { t: "ready", c: "lp-ok" }] },
];
const TYPE_MS = 30;
const HOLD_MS = 6500;
const FINAL_MS = LINES[LINES.length - 1].at + 1;
const TOTAL_MS = FINAL_MS + HOLD_MS;
const LENGTHS = LINES.map((l) => l.segs.reduce((n, s) => n + s.t.length, 0));

function clip(segs: Seg[], chars: number) {
  let left = chars;
  return segs.map((s, i) => {
    if (left <= 0) return null;
    const text = s.t.slice(0, left);
    left -= s.t.length;
    return (
      <span key={i} className={s.c}>
        {text}
      </span>
    );
  });
}

/** A hero terminal that types one deploy, then holds and loops. It runs only while on screen, visible and past the intro overlay; the server render
 *  and prefers-reduced-motion show the whole transcript at once. */
export function DeployTerminal({ className = "" }: { className?: string }) {
  const [ms, setMs] = useState(FINAL_MS);
  const [live, setLive] = useState(false);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- the server render shows the whole transcript for no-script visitors; scripted visitors see it only once this effect has decided between typing it and showing it
    setLive(true);
    if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
    const el = box.current;
    if (!el) return;
    const html = document.documentElement;
    let inView = false;
    let ready = html.classList.contains("lp-ready");
    let clock = 0;
    let elapsed = 0;
    const tick = () => {
      const now = performance.now();
      elapsed = now - clock;
      if (elapsed > TOTAL_MS) {
        clock = now;
        elapsed = 0;
      }
      setMs(elapsed);
    };
    let timer = 0;
    const sync = () => {
      const run = inView && ready && document.visibilityState === "visible";
      if (run && !timer) {
        clock = performance.now() - elapsed;
        timer = window.setInterval(tick, 40);
      } else if (!run && timer) {
        window.clearInterval(timer);
        timer = 0;
      }
    };
    elapsed = 0;
    setMs(0);
    const io = new IntersectionObserver(([e]) => {
      inView = e.isIntersecting;
      sync();
    }, { threshold: 0.25 });
    io.observe(el);
    const mo = new MutationObserver(() => {
      ready = html.classList.contains("lp-ready");
      sync();
    });
    mo.observe(html, { attributes: true, attributeFilter: ["class"] });
    document.addEventListener("visibilitychange", sync);
    sync();
    return () => {
      window.clearInterval(timer);
      io.disconnect();
      mo.disconnect();
      document.removeEventListener("visibilitychange", sync);
    };
  }, []);

  const shown = LINES.map((l, i) => (l.typed ? Math.max(0, Math.min(LENGTHS[i], Math.floor((ms - l.at) / TYPE_MS))) : ms >= l.at ? LENGTHS[i] : 0));
  const lastShown = shown.reduce((last, n, i) => (n > 0 ? i : last), 0);

  return (
    <div
      ref={box}
      role="img"
      aria-label="Terminal transcript: deploying a repository with the shadw CLI. The build clones the repository, detects Next.js, uploads the outputs and finishes ready."
      className={`lp-glass ${className}`}
    >
      <div className="lp-glass-bar" aria-hidden="true">
        <i />
        <i />
        <i />
        <em>Illustrative</em>
      </div>
      <div className="lp-term-body lp-mono" aria-hidden="true" data-live={live ? "1" : "0"} style={{ minHeight: "15.2em" }}>
        {LINES.map((l, i) =>
          shown[i] > 0 ? (
            <div key={i} className={l.gap ? "mt-2" : undefined} style={l.hang ? { paddingLeft: "2ch", textIndent: "-2ch" } : undefined}>
              {clip(l.segs, shown[i])}
              {i === lastShown && <span className="lp-cursor" />}
            </div>
          ) : null,
        )}
      </div>
    </div>
  );
}
