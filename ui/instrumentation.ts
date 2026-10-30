export async function register() {
  const runtime = process.env.NEXT_RUNTIME ?? "nodejs";
  console.log(`[instrumentation] OpenEdge dashboard ready · runtime=${runtime}`);
}

export function onRequestError(err: unknown) {
  console.error("[instrumentation] request error:", err);
}
