import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { ProjectLogs as ProjectLogsClient } from "./project-logs-client";

export default function ProjectLogs(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <ProjectLogsClient paramsPromise={props.params} />
    </Suspense>
  );
}
