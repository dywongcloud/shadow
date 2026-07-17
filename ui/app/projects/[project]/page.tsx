import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { ProjectDetail as ProjectDetailClient } from "./project-detail-client";

export default function ProjectDetail(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <ProjectDetailClient paramsPromise={props.params} />
    </Suspense>
  );
}
