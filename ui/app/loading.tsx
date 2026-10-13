import { Skeleton } from "@/components/ui";

/** The ROOT loading boundary, Next's fallback for every route without its own `loading.tsx`. It must stay generic: there are no route groups, so a dashboard-shaped skeleton
 *  would flash on /docs and /sign-in. Heavy data-driven routes ship their own accurately-shaped `loading.tsx`, which takes precedence. */
export default function Loading() {
  return (
    <div className="flex flex-col gap-4 pb-24">
      <Skeleton className="h-7 w-56" />
      <Skeleton className="h-4 w-80 max-w-full" />
      <Skeleton className="mt-2 h-40 w-full" />
    </div>
  );
}
