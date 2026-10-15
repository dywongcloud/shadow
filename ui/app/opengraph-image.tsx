import { ImageResponse } from "next/og";

export const alt = "shadw — push to Git, get a live URL";
export const size = { width: 1200, height: 630 };
export const contentType = "image/png";

/** The landing's dome in miniature (the same limb gradient, halo strokes, meridians and sample nodes as `DomeKeystone`), as an SVG data URI: Satori draws an
 *  <img>, so gradients and clip paths are the rasteriser's, not its CSS subset's. Nothing here is a blur: the glow is stacked strokes. */
const ARC = "M20 400C20 150 180 -30 380 -30C580 -30 740 150 740 400";
const NODES: [number, number][] = [[140, 300], [250, 205], [335, 290], [395, 95], [505, 190], [590, 300], [675, 240], [305, 35], [465, 350]];
const EDGES = ["M140 300L250 205", "M250 205L335 290", "M250 205L305 35", "M305 35L395 95", "M395 95L505 190", "M505 190L590 300", "M505 190L675 240", "M335 290L465 350", "M335 290L395 95", "M590 300L675 240"];
const MERIDIANS = ["M380 -30V400", "M380 -30C300 40 250 200 240 400", "M380 -30C460 40 510 200 520 400", "M380 -30C230 30 130 210 100 400", "M380 -30C530 30 630 210 660 400"];

const DOME_SVG = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 -70 760 470" width="760" height="470">
<defs>
<linearGradient id="l" gradientUnits="userSpaceOnUse" x1="20" y1="0" x2="740" y2="0"><stop offset="0" stop-color="#8b5cf6" stop-opacity=".7"/><stop offset=".3" stop-color="#60a5fa"/><stop offset=".5" stop-color="#e0f2fe"/><stop offset=".7" stop-color="#38bdf8"/><stop offset="1" stop-color="#22d3ee" stop-opacity=".7"/></linearGradient>
<clipPath id="c"><path d="${ARC}V400H20Z"/></clipPath>
</defs>
<path d="${ARC}V400H20Z" fill="#07090f" fill-opacity=".94"/>
<g clip-path="url(#c)" fill="none" stroke="#ffffff" stroke-opacity=".13" stroke-width="1.2">${[150, 250, 340].map((y) => `<path d="M0 ${y}H760"/>`).join("")}${MERIDIANS.map((d) => `<path d="${d}"/>`).join("")}</g>
<g clip-path="url(#c)" fill="none" stroke="#93c5fd" stroke-opacity=".4" stroke-width="1.2">${EDGES.map((d) => `<path d="${d}"/>`).join("")}</g>
<g fill="none" stroke="url(#l)" stroke-linecap="round">
<path d="${ARC}" stroke-width="44" stroke-opacity=".04"/><path d="${ARC}" stroke-width="26" stroke-opacity=".07"/><path d="${ARC}" stroke-width="14" stroke-opacity=".13"/><path d="${ARC}" stroke-width="7" stroke-opacity=".26"/><path d="${ARC}" stroke-width="3"/>
</g>
${NODES.map(([x, y], i) => `<g transform="translate(${x - 12} ${y - 12})"><rect width="24" height="24" rx="5.5" fill="#000" stroke="${i === 3 ? "#5eead4" : "#2f6bff"}" stroke-width="1.7"/><rect x="6.5" y="6.5" width="11" height="11" rx="2" fill="none" stroke="#e5e7eb" stroke-width="1.4"/></g>`).join("")}
</svg>`;
const DOME_URI = `data:image/svg+xml;base64,${Buffer.from(DOME_SVG).toString("base64")}`;

/** Default social / AI-search preview card (1200×630) for every page that declares none: self-contained (system font, no external fetch) so it builds under the self-hosted node runtime. */
export default function OpengraphImage() {
  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          position: "relative",
          background: "#0c0d10",
          color: "#ffffff",
          fontFamily: "sans-serif",
        }}
      >
        <div
          style={{
            position: "absolute",
            top: 0,
            left: 0,
            width: "100%",
            height: "100%",
            display: "flex",
            backgroundImage:
              "radial-gradient(circle at 12% 0%, rgba(99,102,241,0.5) 0%, rgba(99,102,241,0) 46%), radial-gradient(circle at 86% 100%, rgba(34,211,238,0.3) 0%, rgba(34,211,238,0) 44%)",
          }}
        />
        <img src={DOME_URI} width={660} height={408} alt="" style={{ position: "absolute", right: -30, bottom: 0 }} />
        <div style={{ position: "absolute", top: 0, left: 0, width: 640, height: "100%", display: "flex", flexDirection: "column", justifyContent: "center", padding: "0 0 0 84px" }}>
          <div style={{ fontSize: 58, fontWeight: 700, letterSpacing: "-0.03em", color: "#e0f2fe" }}>shadw</div>
          <div style={{ display: "flex", flexDirection: "column", marginTop: 26, fontSize: 70, lineHeight: 1.04, fontWeight: 700, letterSpacing: "-0.03em" }}>
            <div>Push to Git.</div>
            <div>Get a live URL.</div>
          </div>
          <div style={{ marginTop: 30, fontSize: 28, lineHeight: 1.4, color: "#b8b8c2", maxWidth: 520 }}>
            Serverless functions, containers and static sites, plus managed databases.
          </div>
          <div style={{ marginTop: 44, fontSize: 18, letterSpacing: "0.22em", textTransform: "uppercase", color: "#9a9aa6" }}>Beyond the Edge are Shadows</div>
        </div>
      </div>
    ),
    { ...size },
  );
}
