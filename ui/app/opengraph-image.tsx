import { ImageResponse } from "next/og";

// Default social / AI-search preview card (1200×630) for every page that doesn't
// declare its own. Self-contained (system font, no external fetch) so it builds
// under the self-hosted node runtime.
export const alt = "Autheo DevHub — electric peer-to-peer cloud";
export const size = { width: 1200, height: 630 };
export const contentType = "image/png";

export default function OpengraphImage() {
  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          flexDirection: "column",
          alignItems: "flex-start",
          justifyContent: "center",
          background: "#050b07",
          color: "#ffffff",
          padding: "80px",
          fontFamily: "sans-serif",
        }}
      >
        <div style={{ fontSize: 128, fontWeight: 700, letterSpacing: "-0.04em" }}>autheo.dev</div>
        <div style={{ fontSize: 42, color: "#86efac", marginTop: 20 }}>DevHub electric cloud platform</div>
        <div style={{ fontSize: 28, color: "#6b7075", marginTop: 44, maxWidth: 900 }}>
          A peer-to-peer cloud — serverless functions, containers, edge routing &amp; durable data over an Iroh QUIC mesh.
        </div>
      </div>
    ),
    { ...size },
  );
}
