import { Skeleton, SkeletonPageHeader, SkeletonTable } from "@/components/ui";

// Overrides the generic root loading.tsx — shaped like the default "Runs" tab:
// PageHeader, the status-chip strip, then the runs table.
export default function Loading() {
  return (
    <div>
      <SkeletonPageHeader />
      <Skeleton className="mb-4 h-8 w-64" />
      <SkeletonTable rows={7} />
    </div>
  );
}
