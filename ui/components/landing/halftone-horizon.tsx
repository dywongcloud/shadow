"use client";

import { useEffect, useRef } from "react";

const SOURCE = "/horizon-source.webp";
const SPACING = 7;
const FLOOR = 0.14;

/** A dot-matrix horizon above the marketing footer. A blue-lit arc, two dashed
 *  orbital paths, a vertical beam and node squares are sampled into a 7 px dot
 *  grid: luminance drives radius, alpha and colour. The source is our own
 *  geometry (no rival badge); it is only a sampling source and is never shown.
 *  Draws once when motion is off, and a 30 fps shimmer only while visible,
 *  unpaused and unhidden -- the landing's ONE motion state is html.lp-paused. */
export function HalftoneHorizon() {
  const hostRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const host = hostRef.current;
    const canvas = canvasRef.current;
    if (!host || !canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const image = new window.Image();
    image.decoding = "async";
    let cols = 0;
    let rows = 0;
    let width = 0;
    let height = 0;
    let dpr = 1;
    let intensity = new Float32Array(0);
    let frame = 0;
    let last = 0;
    let born = 0;
    let visible = false;
    let disposed = false;
    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)");

    const sample = () => {
      if (!image.complete || !image.naturalWidth || !cols || !rows) return;
      const off = document.createElement("canvas");
      off.width = cols;
      off.height = rows;
      const offCtx = off.getContext("2d", { willReadFrequently: true });
      if (!offCtx) return;
      // cover, not stretch: the arc keeps its curve at every viewport width
      const scale = Math.max(cols / image.naturalWidth, rows / image.naturalHeight);
      const sw = cols / scale;
      const sh = rows / scale;
      const sx = (image.naturalWidth - sw) / 2;
      const sy = (image.naturalHeight - sh) / 2;
      offCtx.drawImage(image, sx, sy, sw, sh, 0, 0, cols, rows);
      const px = offCtx.getImageData(0, 0, cols, rows).data;
      intensity = new Float32Array(cols * rows);
      for (let i = 0; i < cols * rows; i++) {
        const lum = (0.2126 * px[4 * i] + 0.7152 * px[4 * i + 1] + 0.0722 * px[4 * i + 2]) / 255;
        intensity[i] = Math.max(0, Math.min(1, 1.35 * Math.sqrt(lum) - FLOOR));
      }
    };

    const draw = (time: number) => {
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, width, height);
      if (!intensity.length) return;
      const maxR = 0.48 * SPACING;
      const seconds = (time - born) / 1000;
      for (let row = 0; row < rows; row++) {
        for (let col = 0; col < cols; col++) {
          const v = intensity[row * cols + col];
          if (v < 0.13) continue;
          const shimmer = time === 0 ? 1 : 0.78 + 0.22 * Math.sin(2.2 * seconds - 0.18 * col + 0.06 * row);
          const radius = Math.min(maxR, maxR * v * shimmer);
          if (radius < 0.35) continue;
          ctx.fillStyle = v > 0.62 ? "#ffffff" : v > 0.34 ? "#8fd8ff" : "#2a8af6";
          ctx.globalAlpha = Math.min(1, 0.35 + v);
          ctx.beginPath();
          ctx.arc(col * SPACING + SPACING / 2, row * SPACING + SPACING / 2, radius, 0, Math.PI * 2);
          ctx.fill();
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
      cols = Math.max(1, Math.floor(width / SPACING));
      rows = Math.max(1, Math.floor(height / SPACING));
      sample();
      draw(0);
    };

    const tick = (time: number) => {
      if (disposed) return;
      frame = requestAnimationFrame(tick);
      if (time - last < 1000 / 30) return;
      last = time;
      draw(time);
    };

    const motionAllowed = () =>
      visible && !document.hidden && !reduced.matches && !document.documentElement.classList.contains("lp-paused");

    const sync = () => {
      if (disposed) return;
      cancelAnimationFrame(frame);
      if (motionAllowed()) {
        born = performance.now();
        last = 0;
        frame = requestAnimationFrame(tick);
      } else {
        draw(0);
      }
    };

    const onLoad = () => {
      resize();
      sync();
    };
    image.addEventListener("load", onLoad);
    image.src = SOURCE;

    const ro = typeof ResizeObserver !== "undefined" ? new ResizeObserver(() => { resize(); sync(); }) : null;
    ro?.observe(host);
    const io = typeof IntersectionObserver !== "undefined"
      ? new IntersectionObserver((entries) => {
          visible = entries[0]?.isIntersecting ?? false;
          sync();
        }, { threshold: 0.15 })
      : null;
    if (io) io.observe(host);
    else visible = true;
    document.addEventListener("visibilitychange", sync);
    reduced.addEventListener("change", sync);
    window.addEventListener("resize", resize);
    // the landing's one motion state is a class on <html>, toggled from the
    // PauseSkip control; without this observer a visible band keeps shimmering
    const mo = new MutationObserver(sync);
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    sync();

    return () => {
      disposed = true;
      cancelAnimationFrame(frame);
      image.removeEventListener("load", onLoad);
      ro?.disconnect();
      io?.disconnect();
      mo.disconnect();
      document.removeEventListener("visibilitychange", sync);
      reduced.removeEventListener("change", sync);
      window.removeEventListener("resize", resize);
    };
  }, []);

  return (
    <div className="lp-horizon" aria-hidden="true">
      <div ref={hostRef} className="lp-horizon-stage">
        <canvas ref={canvasRef} className="lp-horizon-canvas" />
      </div>
    </div>
  );
}
