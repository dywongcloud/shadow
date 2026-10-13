import { Skeleton, SkeletonPageHeader, SkeletonTable } from "@/components/ui";

export default function Loading() {
  return (
    <div>
      <SkeletonPageHeader withAction />
      <SkeletonTable rows={8} />
      <Skeleton className="mt-4 h-4 w-32" />
    </div>
  );
}
