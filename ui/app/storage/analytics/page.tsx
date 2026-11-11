import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import StorageAnalyticsClient from "./analytics-client";

export default function StorageAnalyticsPage() {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <StorageAnalyticsClient />
    </Suspense>
  );
}
