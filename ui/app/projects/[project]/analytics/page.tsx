import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { AnalyticsPageClient } from "./analytics-client";

export default function ProjectAnalyticsPage(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <AnalyticsPageClient paramsPromise={props.params} />
    </Suspense>
  );
}
