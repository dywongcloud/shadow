import { Suspense } from "react";
import { WfConsoleFrame } from "@/components/wf-console-frame";

// ---------------------------------------------------------------------------
// /workflows — the workflow observability console, organically inside the
// platform chrome. This page is now a SERVER COMPONENT (SSR): the shell and
// the route params are resolved on the server and streamed as HTML, so the
// page is server-rendered rather than a client-only bundle. The interactive
// console embed (WfConsoleFrame — theme/session/URL-sync + the isolating
// iframe) is the ONLY client island; it stays "use client" and hydrates on
// top of the server-rendered shell. The Suspense boundary is required because
// WfConsoleFrame reads useSearchParams (client hook) — Next needs the
// boundary to server-render the shell without bailing the whole route to CSR.
//
// Catch-all so deep links (/workflows/run/<id>) open the matching console view
// directly; /workflows/runs/<id> legacy links are redirected by next.config.
// ---------------------------------------------------------------------------

export default async function WorkflowsPage({
  params,
}: {
  params: Promise<{ path?: string[] }>;
}) {
  const { path } = await params;
  const sub = (path ?? []).join("/");
  return (
    <Suspense fallback={<div className="h-40" />}>
      <WfConsoleFrame initialPath={sub} syncUrl bleed />
    </Suspense>
  );
}
