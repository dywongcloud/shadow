import { Skeleton, SkeletonPageHeader } from "@/components/ui";

// Streaming shell for /integrations (the only audited page that lacked one):
// PageHeader + the marketplace card grid, so navigation paints instantly while
// the client bundle hydrates and the toolkit catalog loads.
export default function Loading() {
  return (
    <div>
      <SkeletonPageHeader />
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {Array.from({ length: 6 }).map((_, i) => (
          <Skeleton key={i} className="h-36 w-full" />
        ))}
      </div>
    </div>
  );
}
