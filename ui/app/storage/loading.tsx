import { SkeletonPageHeader, SkeletonTable } from "@/components/ui";

// Overrides the generic root loading.tsx — shaped like the real page:
// PageHeader (+ "Create Database" action) followed by the databases table.
export default function Loading() {
  return (
    <div className="pb-24">
      <SkeletonPageHeader withAction />
      <SkeletonTable rows={5} />
    </div>
  );
}
