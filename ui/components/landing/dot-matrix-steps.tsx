"use client";

import { useEffect, useRef, useState } from "react";
import { Section, SectionHead } from "./section";
import { dotColor, paintScene, sampleIntensity, vortex } from "./dot-matrix";

/** Four visitor-chosen states of one push. Every line names a feature the code
 *  has (connected-repository builds, per-PR previews, domains/DNS/TLS, logs and
 *  metrics); none names architecture, a count, a duration or a price. The
 *  illustration prints no step number and no annotation label. */
const STEPS = [
  { label: "Push to Git", title: "Push to Git", body: "A connected repository builds on every push." },
  { label: "Get a live URL", title: "Get a live URL", body: "Each pull request gets its own preview, apart from production." },
  { label: "Attach a domain", title: "Attach a domain", body: "Custom domains, DNS records and HTTPS certificates, from the dashboard or the CLI." },
  { label: "Watch it run", title: "Watch it run", body: "Build and runtime logs, and metrics polled from the dashboard, in one place." },
] as const;

const SRC_W = 1200;
const SRC_H = 680;
const COLS = 118;

/** ONE dot-matrix renderer for the landing, shared with the footer horizon.
 *  The source is painted in code (our blue node squares, arcs, plates and
 *  dotted globe) -- no rival artwork, no badge, no downloaded image. The
 *  swirl centre follows the pointer; with no pointer it travels slowly.
 *  Steps change ONLY when the visitor chooses one; there is no autoplay, so a
 *  paused or reduced-motion page is a still halftone, not a stalled film. */
export function DotMatrixSteps() {
  const [step, setStep] = useState(0);
  const hostRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const repaintRef = useRef<((next: number) => void) | null>(null);

  useEffect(() => {
    const host = hostRef.current;
    const canvas = canvasRef.current;
    if (!host || !canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    // paint at full resolution so 1-3 px strokes survive; downsample to the
    // dot grid for luminance sampling. One source canvas, one grid canvas.
    const src = document.createElement("canvas");
    src.width = SRC_W; src.height = SRC_H;
    const srcCtx = src.getContext("2d", { willReadFrequently: true });
    const grid = document.createElement("canvas");
    const gridCtx = grid.getContext("2d", { willReadFrequently: true });
    if (!srcCtx || !gridCtx) return;

    let cols = COLS; let rows = 0; let width = 0; let height = 0; let dpr = 1;
    let intensity = new Float32Array(0);
    let frame = 0; let last = 0; let born = 0;
    let visible = false; let disposed = false;
    let pointerIdle = 0;
    const pointer = { x: 0, y: 0, active: false };
    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)");

    const repaint = (next: number) => {
      rows = Math.max(1, Math.round(height / (width / cols)));
      grid.width = cols; grid.height = rows;
      srcCtx.setTransform(1, 0, 0, 1, 0, 0);
      paintScene(srcCtx, SRC_W, SRC_H, next);
      gridCtx.clearRect(0, 0, cols, rows);
      gridCtx.drawImage(src, 0, 0, cols, rows);
      intensity = sampleIntensity(gridCtx.getImageData(0, 0, cols, rows).data, cols * rows);
    };

    const draw = (time: number) => {
      // ResizeObserver delivery can be delayed (a hidden tab suspends the
      // rendering lifecycle entirely). Re-read the box each frame so the
      // backing store is never scaled down by CSS on a narrow viewport.
      const box = host.getBoundingClientRect();
      const bw = Math.max(1, Math.round(box.width));
      const bh = Math.max(1, Math.round(box.height));
      if (bw !== width || bh !== height) { resize(); return; }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, width, height);
      if (!intensity.length) return;
      const sx = width / cols; const sy = height / rows;
      const seconds = (time - born) / 1000;
      // A stationary page is a still halftone. The swirl is visitor-driven:
      // it follows the pointer and settles when the pointer stops. No idle
      // Lissajous path -- the reference recordings show the vortex tracking
      // the cursor, not a self-moving spectacle.
      const still = time === 0 || !pointer.active;
      const cx = pointer.active ? pointer.x : width * 0.5;
      const cy = pointer.active ? pointer.y : height * 0.56;
      const sigma = Math.min(width, height) * 0.34;
      const maxR = 0.47 * Math.min(sx, sy);
      // faint base lattice: the ASCII texture stays visible in the dark
      // field, so the picture reads as a halftone screen, not isolated dots.
      ctx.fillStyle = "#173d78";
      ctx.globalAlpha = 0.34;
      for (let row = 0; row < rows; row++) {
        for (let col = 0; col < cols; col++) {
          const x = col * sx + sx / 2; const y = row * sy + sy / 2;
          const w = still ? { x, y } : vortex(x, y, cx, cy, seconds, sigma);
          ctx.beginPath(); ctx.arc(w.x, w.y, 0.62, 0, Math.PI * 2); ctx.fill();
        }
      }
      ctx.globalAlpha = 1;
      // The frame has no CSS corner ticks: the corners are where the SAME dot
      // lattice thickens, so the bracket is made of the picture's own medium.
      for (const [ox, oy] of [[0, 0], [cols - 1, 0], [0, rows - 1], [cols - 1, rows - 1]]) {
        for (let dy = 0; dy < 4; dy++) for (let dx = 0; dx < 4; dx++) {
          const cc = ox + (ox === 0 ? dx : -dx);
          const rr = oy + (oy === 0 ? dy : -dy);
          if (cc < 0 || cc >= cols || rr < 0 || rr >= rows) continue;
          const v = intensity[rr * cols + cc];
          ctx.fillStyle = dotColor(Math.max(0.72, v));
          ctx.globalAlpha = 0.9;
          ctx.beginPath();
          ctx.arc(cc * sx + sx / 2, rr * sy + sy / 2, 1.5, 0, Math.PI * 2);
          ctx.fill();
        }
      }
      ctx.globalAlpha = 1;
      for (let row = 0; row < rows; row++) {
        for (let col = 0; col < cols; col++) {
          const v = intensity[row * cols + col];
          if (v < 0.13) continue;
          const x = col * sx + sx / 2; const y = row * sy + sy / 2;
          const w = still ? { x, y, wave: 0 } : vortex(x, y, cx, cy, seconds, sigma);
          const lift = Math.min(1, v + 0.42 * w.wave * v);
          const radius = Math.min(maxR, maxR * lift * (still ? 1 : 0.84 + 0.16 * Math.sin(2.1 * seconds - 0.15 * col)));
          if (radius < 0.35) continue;
          ctx.fillStyle = dotColor(lift);
          ctx.globalAlpha = Math.min(1, 0.34 + lift);
          ctx.beginPath(); ctx.arc(w.x, w.y, radius, 0, Math.PI * 2); ctx.fill();
        }
      }
      ctx.globalAlpha = 1;
    };

    const resize = () => {
      const rect = host.getBoundingClientRect();
      width = Math.max(1, Math.round(rect.width));
      height = Math.max(1, Math.round(rect.height));
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
      repaint(0); draw(0);
    };

    const tick = (time: number) => {
      if (disposed) return;
      frame = requestAnimationFrame(tick);
      if (time - last < 1000 / 30) return;
      last = time; draw(time);
    };

    const motionAllowed = () =>
      visible && !document.hidden && !reduced.matches && pointer.active &&
      !document.documentElement.classList.contains("lp-paused");

    const sync = () => {
      if (disposed) return;
      cancelAnimationFrame(frame);
      if (motionAllowed()) {
        born = performance.now(); last = 0;
        frame = requestAnimationFrame(tick);
      } else draw(0);
    };

    const onPointer = (e: PointerEvent) => {
      const r = host.getBoundingClientRect();
      pointer.x = e.clientX - r.left;
      pointer.y = e.clientY - r.top;
      const wasActive = pointer.active;
      pointer.active = true;
      window.clearTimeout(pointerIdle);
      pointerIdle = window.setTimeout(() => { pointer.active = false; sync(); }, 1600);
      if (!wasActive) sync();
    };
    const onLeave = () => {
      window.clearTimeout(pointerIdle);
      pointer.active = false;
      sync();
    };
    host.addEventListener("pointermove", onPointer, { passive: true });
    host.addEventListener("pointerleave", onLeave);

    const ro = typeof ResizeObserver !== "undefined" ? new ResizeObserver(() => { resize(); sync(); }) : null;
    ro?.observe(host);
    const io = typeof IntersectionObserver !== "undefined"
      ? new IntersectionObserver((entries) => { visible = entries[0]?.isIntersecting ?? false; sync(); }, { threshold: 0.12 })
      : null;
    if (io) io.observe(host); else visible = true;
    document.addEventListener("visibilitychange", sync);
    reduced.addEventListener("change", sync);
    // the landing's ONE motion state is a class on <html>, toggled by PauseSkip
    const mo = new MutationObserver(sync);
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });

    repaintRef.current = (next: number) => { repaint(next); draw(0); };
    resize(); sync();

    return () => {
      disposed = true;
      window.clearTimeout(pointerIdle);
      repaintRef.current = null;
      cancelAnimationFrame(frame);
      host.removeEventListener("pointermove", onPointer);
      host.removeEventListener("pointerleave", onLeave);
      ro?.disconnect(); io?.disconnect(); mo.disconnect();
      document.removeEventListener("visibilitychange", sync);
      reduced.removeEventListener("change", sync);
    };
  }, []);

  const choose = (next: number) => {
    setStep(next);
    repaintRef.current?.(next);
  };

  return (
    <Section className="pt-28 sm:pt-36" wash="duo">
      <SectionHead
        eyebrow="From one repository"
        title={<>One push, every step <span className="lp-gradient-text">in view.</span></>}
        lead="Choose a step to see the build, the preview, the domain and the logs."
      />
      <div className="lp-dots-frame mt-14">
        <div ref={hostRef} className="lp-dots-stage">
          <canvas ref={canvasRef} className="lp-dots-canvas" aria-hidden="true" />
          <div className="lp-dots-grid" aria-hidden="true" />
        </div>
        <div className="lp-dots-foot">
          <div className="lp-dots-tabs" role="group" aria-label="Choose a deployment step">
            {STEPS.map((s, i) => (
              <button
                key={s.label}
                type="button"
                onClick={() => choose(i)}
                aria-current={i === step ? "step" : undefined}
                className={`lp-dots-tab tap-44 inline-flex min-h-[44px] items-center ${i === step ? "is-active" : ""}`}
              >
                {s.label}
              </button>
            ))}
          </div>
          <p className="lp-dots-copy lp-caption">{STEPS[step].body}</p>
        </div>
        <span className="sr-only" role="status" aria-live="polite">{`${STEPS[step].title}. ${STEPS[step].body}`}</span>
      </div>
      <ol className="lp-dots-fallback">
        {STEPS.map((s) => <li key={s.label}><strong>{s.title}</strong>. {s.body}</li>)}
      </ol>
    </Section>
  );
}
