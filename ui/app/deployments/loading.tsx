import { Skeleton, SkeletonPageHeader, SkeletonTable } from "@/components/ui";

// Overrides the generic root loading.tsx — shaped like the real page:
// PageHeader (+ "New Deployment" action) followed by the deployments table.
export default function Loading() {
  return (
    <div>
      <SkeletonPageHeader withAction />
      <SkeletonTable rows={8} />
      <Skeleton className="mt-4 h-4 w-32" />
    </div>
  );
}
