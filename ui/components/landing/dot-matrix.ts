/* One dot-matrix primitive for the landing. The horizon band above the footer
 * and the interactive stepper both sample a painted luminance field into a grid
 * of dots; neither ever shows the source. All geometry is ours (blue node
 * squares, arcs, dashed orbits, plates, a dotted globe), never a third-party
 * badge or a rival artwork. Sizes are px: the dashboard root scales rem. */

export const DOT_SPACING = 8;
export const DOT_FLOOR = 0.14;
export const DOT_WHITE = "#ffffff";
export const DOT_ICE = "#8fd8ff";
export const DOT_BLUE = "#2a8af6";

/** Luminance -> dot intensity. 1.35*sqrt keeps dark blue structure visible
 *  without flooding; FLOOR removes the black background. Unchanged from the
 *  horizon renderer so the band's rendered pixels do not drift. */
export function sampleIntensity(px: Uint8ClampedArray, cells: number): Float32Array {
  const out = new Float32Array(cells);
  for (let i = 0; i < cells; i++) {
    const lum = (0.2126 * px[4 * i] + 0.7152 * px[4 * i + 1] + 0.0722 * px[4 * i + 2]) / 255;
    out[i] = Math.max(0, Math.min(1, 1.35 * Math.sqrt(lum) - DOT_FLOOR));
  }
  return out;
}

export function dotColor(v: number): string {
  return v > 0.62 ? DOT_WHITE : v > 0.34 ? DOT_ICE : DOT_BLUE;
}

/** A traveling vortex: nearby dots rotate around the centre and ripple
 *  radially, so the straight grid bends into curved streamlines and bright
 *  petals. Far dots keep their lattice, which is what makes the reference
 *  read as a halftone image rather than a particle cloud. */
export function vortex(
  x: number, y: number, cx: number, cy: number, seconds: number, sigma: number,
): { x: number; y: number; wave: number } {
  const dx = x - cx;
  const dy = y - cy;
  const r = Math.sqrt(dx * dx + dy * dy) + 0.0001;
  const falloff = Math.exp(-(r * r) / (2 * sigma * sigma));
  const theta = Math.atan2(dy, dx);
  const twist = 1.55 * falloff * (0.62 + 0.38 * Math.sin(0.85 * seconds - r * 0.011));
  const r2 = r * (1 + 0.055 * falloff * Math.sin(1.5 * seconds - r * 0.032));
  const ring = Math.exp(-Math.pow((r - ((seconds * 82) % 330)) / 30, 2));
  const petal = 0.5 + 0.5 * Math.cos(3 * theta + 0.7 * seconds);
  return {
    x: cx + r2 * Math.cos(theta + twist),
    y: cy + r2 * Math.sin(theta + twist),
    wave: falloff * (0.55 * ring + 0.45 * petal),
  };
}

/** Four luminance scenes, painted with gradients and stroke stacks only (no
 *  filter blur). Each is a real shadw outcome: Git to a live URL, a preview
 *  per PR, custom domains with DNS/TLS, and build/runtime logs with metrics.
 *  Coordinates are a virtual 1200x680 space; the caller scales to the grid. */
export function paintScene(ctx: CanvasRenderingContext2D, w: number, h: number, step: number): void {
  ctx.save();
  ctx.scale(w / 1200, h / 680);
  ctx.fillStyle = "#04060b";
  ctx.fillRect(0, 0, 1200, 680);

  const glow = ctx.createRadialGradient(600, 340, 0, 600, 340, 430);
  glow.addColorStop(0, "rgba(42,138,246,.30)");
  glow.addColorStop(0.55, "rgba(20,70,160,.10)");
  glow.addColorStop(1, "rgba(0,0,0,0)");
  ctx.fillStyle = glow;
  ctx.fillRect(0, 0, 1200, 680);

  const strokeGrad = ctx.createLinearGradient(120, 0, 1080, 0);
  strokeGrad.addColorStop(0, "#2a8af6");
  strokeGrad.addColorStop(0.5, "#dff2ff");
  strokeGrad.addColorStop(1, "#2a8af6");

  const node = (x: number, y: number, s: number, bright: boolean) => {
    ctx.fillStyle = bright ? "#eaf7ff" : "#3fa2f7";
    ctx.fillRect(x - s / 2, y - s / 2, s, s);
    ctx.strokeStyle = "#ffffff";
    ctx.lineWidth = 1.2;
    ctx.strokeRect(x - s / 2, y - s / 2, s, s);
  };

  const arc = (x0: number, y0: number, x1: number, y1: number, bend: number) => {
    ctx.beginPath();
    ctx.moveTo(x0, y0);
    ctx.quadraticCurveTo((x0 + x1) / 2, bend, x1, y1);
    ctx.strokeStyle = strokeGrad;
    ctx.lineWidth = 3.4;
    ctx.stroke();
    ctx.save();
    ctx.setLineDash([2, 9]);
    ctx.lineWidth = 1.1;
    ctx.strokeStyle = "rgba(223,242,255,.55)";
    ctx.stroke();
    ctx.restore();
  };

  const panel = (x: number, y: number, pw: number, ph: number) => {
    ctx.beginPath();
    ctx.roundRect(x, y, pw, ph, 14);
    ctx.fillStyle = "rgba(6,14,30,.94)";
    ctx.fill();
    ctx.strokeStyle = "rgba(143,216,255,.55)";
    ctx.lineWidth = 1.4;
    ctx.stroke();
  };

  if (step === 0) {
    // One push, one live URL: five node squares feed a central hub and a bar panel.
    const sources = [[175, 260], [370, 190], [600, 155], [830, 190], [1025, 260]];
    for (const [x, y] of sources) {
      arc(x, y + 42, 600, 400, y > 220 ? 470 : 430);
      ctx.fillStyle = "rgba(8,18,36,.95)";
      ctx.fillRect(x - 46, y - 62, 92, 124);
      ctx.strokeStyle = "rgba(223,242,255,.75)";
      ctx.lineWidth = 1.6;
      ctx.strokeRect(x - 46, y - 62, 92, 124);
      for (let i = 0; i < 5; i++) {
        ctx.fillStyle = "rgba(143,216,255,.30)";
        ctx.fillRect(x - 34, y - 48 + i * 22, 68, 8);
        node(x + 26, y - 44 + i * 22, 5, i === 2);
      }
      node(x, y - 62, 8, true);
    }
    ctx.beginPath();
    ctx.moveTo(600, 155); ctx.lineTo(600, 375);
    ctx.strokeStyle = strokeGrad; ctx.lineWidth = 3.4; ctx.stroke();
    ctx.beginPath();
    ctx.arc(600, 400, 46, 0, Math.PI * 2);
    ctx.strokeStyle = "#cfe9ff"; ctx.lineWidth = 5; ctx.stroke();
    const core = ctx.createRadialGradient(600, 400, 0, 600, 400, 30);
    core.addColorStop(0, "#ffffff"); core.addColorStop(1, "#2a8af6");
    ctx.fillStyle = core;
    ctx.beginPath(); ctx.arc(600, 400, 27, 0, Math.PI * 2); ctx.fill();
    ctx.beginPath(); ctx.moveTo(600, 446); ctx.lineTo(600, 505);
    ctx.strokeStyle = strokeGrad; ctx.lineWidth = 3; ctx.stroke();
    panel(470, 505, 260, 110);
    for (let i = 0; i < 6; i++) {
      const bh = 26 + i * 11;
      ctx.fillStyle = i > 3 ? "#eaf7ff" : "#3fa2f7";
      ctx.fillRect(490 + i * 38, 595 - bh, 22, bh);
    }
  } else if (step === 1) {
    // A preview per pull request: four stacked isometric plates.
    const plate = (cy: number, alpha: number, top: boolean) => {
      ctx.beginPath();
      ctx.moveTo(600, cy - 92); ctx.lineTo(1010, cy + 24);
      ctx.lineTo(600, cy + 140); ctx.lineTo(190, cy + 24); ctx.closePath();
      ctx.fillStyle = top ? "rgba(42,138,246,.72)" : `rgba(10,20,38,${alpha})`;
      ctx.fill();
      ctx.strokeStyle = top ? "#dff2ff" : "rgba(143,216,255,.42)";
      ctx.lineWidth = top ? 2.4 : 1.4; ctx.stroke();
    };
    plate(470, 0.95, false); plate(390, 0.95, false);
    plate(310, 0.95, false); plate(210, 0.95, true);
    node(600, 210, 30, true);
    for (let i = 0; i < 3; i++) for (let j = 0; j < 3; j++) node(572 + j * 28, 300 + i * 20, 7, i === j);
    ctx.beginPath(); ctx.arc(600, 400, 24, 0, Math.PI * 2);
    ctx.strokeStyle = "#8fd8ff"; ctx.lineWidth = 3; ctx.stroke();
    ctx.strokeRect(566, 482, 68, 46);
  } else if (step === 2) {
    // Domains, DNS and TLS: a dotted globe with lit pins and a record panel.
    ctx.save();
    ctx.beginPath(); ctx.arc(600, 315, 220, 0, Math.PI * 2); ctx.clip();
    for (let y = 105; y < 530; y += 14) {
      for (let x = 390; x < 815; x += 14) {
        const edge = Math.hypot(x - 600, y - 315) / 220;
        const lum = Math.max(0, 1 - edge) * (0.55 + 0.45 * Math.sin(x * 0.021 + y * 0.017));
        if (lum < 0.16) continue;
        ctx.fillStyle = lum > 0.62 ? "#dff2ff" : "#2a8af6";
        ctx.globalAlpha = 0.35 + lum * 0.65;
        ctx.beginPath(); ctx.arc(x, y, 2.6, 0, Math.PI * 2); ctx.fill();
      }
    }
    ctx.globalAlpha = 1; ctx.restore();
    ctx.beginPath(); ctx.arc(600, 315, 220, 0, Math.PI * 2);
    ctx.strokeStyle = "rgba(143,216,255,.5)"; ctx.lineWidth = 2; ctx.stroke();
    for (const [x, y] of [[470, 245], [520, 360], [625, 205], [700, 330], [760, 410]]) {
      const pin = ctx.createRadialGradient(x, y, 0, x, y, 26);
      pin.addColorStop(0, "rgba(255,255,255,.95)"); pin.addColorStop(1, "rgba(42,138,246,0)");
      ctx.fillStyle = pin; ctx.beginPath(); ctx.arc(x, y, 26, 0, Math.PI * 2); ctx.fill();
      node(x, y, 9, true);
    }
    panel(360, 430, 480, 160);
    for (let i = 0; i < 4; i++) {
      ctx.fillStyle = i === 1 ? "#eaf7ff" : "#3fa2f7";
      ctx.fillRect(385, 455 + i * 33, 130, 13);
      ctx.fillRect(560, 455 + i * 33, i === 1 ? 200 : 160, 13);
    }
  } else {
    // Logs and metrics: a live chart with a rising trace and node squares.
    panel(180, 120, 840, 440);
    for (let i = 0; i < 6; i++) {
      const bh = 60 + i * 44;
      ctx.fillStyle = i > 3 ? "#eaf7ff" : "#3fa2f7";
      ctx.fillRect(230 + i * 125, 500 - bh, 62, bh);
    }
    ctx.beginPath();
    for (let i = 0; i <= 24; i++) {
      const x = 220 + i * 32;
      const y = 300 - 120 * Math.sin(i * 0.27) - 30 * Math.cos(i * 0.11);
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    }
    ctx.strokeStyle = "#dff2ff"; ctx.lineWidth = 3; ctx.stroke();
    for (let i = 0; i < 5; i++) node(240 + i * 180, 176, 9, i === 2);
    for (const [x, y] of [[130, 250], [1070, 250], [130, 470], [1070, 470]]) {
      arc(x, y, x < 600 ? 180 : 1020, y, y < 340 ? 170 : 550);
      node(x, y, 12, true);
    }
  }
  ctx.restore();
}
