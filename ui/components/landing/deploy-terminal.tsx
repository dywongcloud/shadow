"use client";

import { useEffect, useRef, useState } from "react";

type Seg = { t: string; c?: string };
type Line = { at: number; segs: Seg[]; typed?: boolean; gap?: boolean; hang?: boolean };

/** The transcript is the shape `shadw deploy --follow` prints (crates/shadw-cli/src/main.rs `deploy`/`follow_build`) around real build-log
 *  lines (crates/hive-cloud/src/git.rs); the repository, project and deployment id are samples and no duration is printed. */
const ID = "dpl-3f9a21c07b";
const KEEP = "whitespace-nowrap";
const LINES: Line[] = [
  { at: 0, typed: true, segs: [{ t: "$ ", c: "lp-prompt" }, { t: "shadw deploy " }, { t: "https://github.com/you/my-app", c: KEEP }, { t: " \\" }] },
  { at: 1750, typed: true, segs: [{ t: "    " }, { t: "--project", c: KEEP }, { t: " " }, { t: "my-app", c: KEEP }, { t: " " }, { t: "--follow", c: KEEP }] },
  { at: 2900, hang: true, segs: [{ t: "Deploy started: build " }, { t: ID, c: KEEP }, { t: " (project " }, { t: "my-app", c: KEEP }, { t: ")" }] },
  { at: 3200, hang: true, segs: [{ t: "Stream logs:  shadw build logs ", c: "lp-dim" }, { t: ID, c: `lp-dim ${KEEP}` }, { t: " ", c: "lp-dim" }, { t: "--follow", c: `lp-dim ${KEEP}` }] },
  { at: 3900, gap: true, hang: true, segs: [{ t: "Cloning " }, { t: "github.com/you/my-app", c: KEEP }, { t: " (Branch: main, Commit: HEAD)" }] },
  { at: 4600, hang: true, segs: [{ t: "Detected framework: Next.js — primitive: Hybrid, package manager: npm (NpmLock)" }] },
  { at: 5300, hang: true, segs: [{ t: "…", c: "lp-dim" }] },
  { at: 5900, hang: true, segs: [{ t: "Uploading build outputs…" }] },
  { at: 6700, gap: true, hang: true, segs: [{ t: "Build " }, { t: ID, c: KEEP }, { t: " → " }, { t: "ready", c: "lp-ok" }] },
];
const TYPE_MS = 30;
const FINAL_MS = LINES[LINES.length - 1].at + 1;
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

/** A hero terminal that types one deploy once and holds the finished transcript. It types only while on screen, visible and past the intro overlay; the
 *  server render, prefers-reduced-motion and a page that starts paused show the whole transcript at once. */
export function DeployTerminal({ className = "" }: { className?: string }) {
  const [ms, setMs] = useState(FINAL_MS);
  const [live, setLive] = useState(false);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- the server render shows the whole transcript for no-script visitors; scripted visitors see it only once this effect has decided between typing it and showing it
    setLive(true);
    const html = document.documentElement;
    // a page that starts paused (reduced motion, or a Pause the visitor remembered) shows the whole transcript, as the server render does
    if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches || html.classList.contains("lp-paused")) return;
    const el = box.current;
    if (!el) return;
    const dome = el.closest<HTMLElement>(".lp-dome");
    dome?.setAttribute("data-deploy", "building");
    let inView = false;
    let ready = html.classList.contains("lp-ready");
    let clock = 0;
    let elapsed = 0;
    let timer = 0;
    const stop = () => {
      window.clearInterval(timer);
      timer = 0;
    };
    const tick = () => {
      elapsed = Math.min(FINAL_MS, performance.now() - clock);
      setMs(elapsed);
      if (elapsed >= FINAL_MS) {
        dome?.setAttribute("data-deploy", "ready");
        stop();
      }
    };
    const sync = () => {
      const run = inView && ready && !html.classList.contains("lp-paused") && document.visibilityState === "visible" && elapsed < FINAL_MS;
      if (run && !timer) {
        clock = performance.now() - elapsed;
        timer = window.setInterval(tick, 40);
      } else if (!run && timer) {
        stop();
      }
    };
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
      dome?.removeAttribute("data-deploy");
      window.clearInterval(timer);
      io.disconnect();
      mo.disconnect();
      document.removeEventListener("visibilitychange", sync);
    };
  }, []);

  const shown = LINES.map((l, i) => (l.typed ? Math.max(0, Math.min(LENGTHS[i], Math.floor((ms - l.at) / TYPE_MS))) : ms >= l.at ? LENGTHS[i] : 0));
  const lastShown = shown.reduce((last, n, i) => (n > 0 ? i : last), 0);
  const row = (l: Line, i: number, chars: number, cursor: boolean) => (
    <div key={i} className={l.gap ? "mt-2" : undefined} style={l.hang ? { paddingLeft: "2ch", textIndent: "-2ch" } : undefined}>
      {clip(l.segs, chars)}
      {cursor && <span className="lp-cursor" />}
    </div>
  );

  return (
    <div
      ref={box}
      role="img"
      aria-label="Illustrative terminal transcript: deploying a repository with the shadw CLI. The build clones the repository, detects Next.js, uploads the outputs and finishes ready."
      className={`lp-glass ${className}`}
    >
      <div className="lp-glass-bar" aria-hidden="true">
        <i />
        <i />
        <i />
        <em>Illustrative</em>
      </div>
      <div className="lp-term-body lp-mono relative" aria-hidden="true" data-live={live ? "1" : "0"}>
        <div className="invisible">{LINES.map((l, i) => row(l, i, LENGTHS[i], false))}</div>
        <div className="absolute inset-x-0 top-0" style={{ padding: "14px 16px 0" }}>{LINES.map((l, i) => (shown[i] > 0 ? row(l, i, shown[i], i === lastShown) : null))}</div>
      </div>
    </div>
  );
}
