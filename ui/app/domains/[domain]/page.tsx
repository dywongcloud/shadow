import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { DomainDetailPage as DomainDetailPageClient } from "./domain-detail-page-client";

export default function DomainDetailPage(props: { params: Promise<{ domain: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <DomainDetailPageClient paramsPromise={props.params} />
    </Suspense>
  );
}
