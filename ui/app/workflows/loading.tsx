import { Skeleton, SkeletonPageHeader, SkeletonTable } from "@/components/ui";

export default function Loading() {
  return (
    <div>
      <SkeletonPageHeader />
      <Skeleton className="mb-4 h-8 w-64" />
      <SkeletonTable rows={7} />
    </div>
  );
}
