import { SkeletonPageHeader, SkeletonTable } from "@/components/ui";

export default function Loading() {
  return (
    <div className="pb-24">
      <SkeletonPageHeader withAction />
      <SkeletonTable rows={5} />
    </div>
  );
}
