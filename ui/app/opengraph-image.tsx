import { ImageResponse } from "next/og";
import { globeSvgMarkup, GLOBE_STROKE } from "@/components/globe-art";

export const alt = "shadw — push to Git, get a live URL";
export const size = { width: 1200, height: 630 };
export const contentType = "image/png";

/** The /new deployment globe as an SVG data URI: Satori draws an <img>, so
 *  masks, gradients and strokes are the rasteriser's, not its CSS subset's.
 *  `globeSvgMarkup` carries no inline CSS (the asset's display-p3 stroke and
 *  mask-type:alpha are not portable), and the literal stroke is legible on the
 *  black card. `detail` adds the lit horizon, blue links and nine node squares
 *  the old hand-drawn card had; the landing keystone draws the same geometry
 *  without them. The diffuse blue corner wash is gone: its light came from no
 *  object on the page, and it hazed the whole right half of the card. */
const GLOBE_URI = `data:image/svg+xml;base64,${Buffer.from(globeSvgMarkup({ stroke: GLOBE_STROKE, idPrefix: "share", detail: true })).toString("base64")}`;

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
          background: "#000000",
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
              "radial-gradient(circle at 12% 0%, rgba(168, 83, 186,0.5) 0%, rgba(168, 83, 186,0) 46%)",
          }}
        />
        <img src={GLOBE_URI} width={760} height={283} alt="" style={{ position: "absolute", right: -140, bottom: -55 }} />
        <div style={{ position: "absolute", top: 0, left: 0, width: 640, height: "100%", display: "flex", flexDirection: "column", justifyContent: "center", padding: "0 0 0 84px" }}>
          <div style={{ fontSize: 58, fontWeight: 700, letterSpacing: "-0.03em", color: "#ffe3ec" }}>shadw</div>
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
