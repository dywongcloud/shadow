import { Skeleton, SkeletonCard, SkeletonPageHeader } from "@/components/ui";

// Overrides the generic root loading.tsx — shaped like the real page: PageHeader,
// a 4-up stat row, the mesh-diagram card, then a 3-up architecture row.
export default function Loading() {
  return (
    <div>
      <SkeletonPageHeader />
      <div className="mb-6 grid grid-cols-2 gap-4 md:grid-cols-4">
        {Array.from({ length: 4 }).map((_, i) => (
          <SkeletonCard key={i} />
        ))}
      </div>
      <Skeleton className="mb-6 h-64 w-full" />
      <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
        {Array.from({ length: 3 }).map((_, i) => (
          <SkeletonCard key={i} className="h-24" />
        ))}
      </div>
    </div>
  );
}
