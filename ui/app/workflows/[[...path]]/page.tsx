"use client";

import { Suspense, use } from "react";
import { WfConsoleFrame } from "@/components/wf-console-frame";

// ---------------------------------------------------------------------------
// /workflows — the workflow observability console, organically inside the
// platform chrome: this is a NORMAL dashboard page (real shadw navbar from
// the root layout) whose content is the LITERAL upstream @workflow/web
// console (served isolated at /wfc, embedded seamlessly and URL-synced by
// WfConsoleFrame). Catch-all so deep links (/workflows/run/<id>) open the
// matching console view directly; /workflows/runs/<id> legacy links are
// redirected to /workflows/run/<id> by next.config.
// ---------------------------------------------------------------------------

export default function WorkflowsPage({ params }: { params: Promise<{ path?: string[] }> }) {
  const { path } = use(params);
  const sub = (path ?? []).join("/");
  return (
    <Suspense fallback={<div className="h-40" />}>
      <WfConsoleFrame initialPath={sub} syncUrl bleed />
    </Suspense>
  );
}
