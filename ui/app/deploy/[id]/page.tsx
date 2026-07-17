import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { DeployPage as DeployPageClient } from "./deploy-page-client";

export default function DeployPage(props: { params: Promise<{ id: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <DeployPageClient paramsPromise={props.params} />
    </Suspense>
  );
}
