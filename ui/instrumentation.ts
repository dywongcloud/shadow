// Next.js instrumentation — runs once when the server process boots, on both the
// Node.js and Edge runtimes. Use it to register observability / startup hooks.
// https://nextjs.org/docs/pages/guides/instrumentation

export async function register() {
  const runtime = process.env.NEXT_RUNTIME ?? "nodejs";
  // Keep this lightweight and dependency-free; it's a hook point for tracing,
  // metrics exporters, or warmups. Logged once per server start.
  // eslint-disable-next-line no-console
  console.log(`[instrumentation] OpenEdge dashboard ready · runtime=${runtime}`);
}

// Optional: capture server-side request errors centrally (Next 14.2+/15).
export function onRequestError(err: unknown) {
  // eslint-disable-next-line no-console
  console.error("[instrumentation] request error:", err);
}
