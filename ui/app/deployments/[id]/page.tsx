import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { DeploymentDetailPage as DeploymentDetailPageClient } from "./deployment-detail-page-client";

export default function DeploymentDetailPage(props: { params: Promise<{ id: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <DeploymentDetailPageClient paramsPromise={props.params} />
    </Suspense>
  );
}
