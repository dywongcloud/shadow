import { Skeleton, SkeletonCard } from "@/components/ui";

// Overrides the generic root loading.tsx for a project's overview tab (and,
// as a reasonable approximation, its settings/logs sub-routes): the header
// row (name + domain + action buttons), then the Production Deployment card
// and the 3-up stat row below it.
export default function Loading() {
  return (
    <div>
      <div className="mb-6 flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <Skeleton className="h-9 w-9 rounded-full" />
          <div className="flex flex-col gap-1.5">
            <Skeleton className="h-5 w-40" />
            <Skeleton className="h-3.5 w-56" />
          </div>
        </div>
        <div className="flex gap-2">
          <Skeleton className="h-9 w-24" />
          <Skeleton className="h-9 w-32" />
          <Skeleton className="h-9 w-16" />
        </div>
      </div>
      <Skeleton className="mb-6 h-56 w-full" />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
        {Array.from({ length: 3 }).map((_, i) => (
          <SkeletonCard key={i} className="h-32" />
        ))}
      </div>
    </div>
  );
}
