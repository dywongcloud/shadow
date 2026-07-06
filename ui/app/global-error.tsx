"use client";

import { useEffect } from "react";

/**
 * Root error boundary — catches exceptions thrown in the root layout itself
 * (which the per-route error.tsx cannot). Must render its own <html>/<body>.
 * Last line of defense against a fully blank app.
 */
export default function GlobalError({ error, reset }: { error: Error & { digest?: string }; reset: () => void }) {
  useEffect(() => {
    console.error("[global error]", error);
  }, [error]);
  return (
    <html lang="en">
      <body style={{ margin: 0, background: "#0c0d10", color: "#e5e7eb", fontFamily: "ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif" }}>
        <div style={{ minHeight: "100vh", display: "flex", flexDirection: "column", alignItems: "center", justifyContent: "center", gap: 12, padding: 24, textAlign: "center" }}>
          <div style={{ fontSize: 18, fontWeight: 600 }}>The dashboard hit an unexpected error</div>
          <p style={{ maxWidth: 420, fontSize: 13, color: "#9ca3af", margin: 0 }}>{error?.message || "Unexpected error."}</p>
          <button
            onClick={reset}
            style={{ marginTop: 4, background: "#fff", color: "#000", border: 0, borderRadius: 8, padding: "8px 14px", fontSize: 14, fontWeight: 600, cursor: "pointer" }}
          >
            Reload
          </button>
        </div>
      </body>
    </html>
  );
}
