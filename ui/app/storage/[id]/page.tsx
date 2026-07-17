import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { DatabaseDetail as DatabaseDetailClient } from "./database-detail-client";

export default function DatabaseDetail(props: { params: Promise<{ id: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <DatabaseDetailClient paramsPromise={props.params} />
    </Suspense>
  );
}
