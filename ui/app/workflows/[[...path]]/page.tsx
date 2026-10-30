import { Suspense } from "react";
import { WfConsoleFrame } from "@/components/wf-console-frame";

/** /workflows: a server-rendered shell around the console embed (`WfConsoleFrame`, the only client island). The catch-all opens deep links
 *  (`/workflows/run/<id>`) in the matching console view; legacy `/workflows/runs/<id>` links are redirected by next.config. */
export default function WorkflowsPage({
  params,
}: {
  params: Promise<{ path?: string[] }>;
}) {
  return (
    <Suspense fallback={<div className="h-40" />}>
      <WorkflowsConsole params={params} />
    </Suspense>
  );
}

async function WorkflowsConsole({ params }: { params: Promise<{ path?: string[] }> }) {
  const { path } = await params;
  const sub = (path ?? []).join("/");
  return <WfConsoleFrame initialPath={sub} syncUrl bleed />;
}
