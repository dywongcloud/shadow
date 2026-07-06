import { Skeleton } from "@/components/ui";

// The ROOT loading boundary — Next.js uses this as the fallback for EVERY route
// under app/ (marketing pages, docs, sign-in, the dashboard…) that doesn't define
// a more specific `loading.tsx` of its own. There are no route groups in this
// tree (every top-level folder — /docs, /pricing, /admin, /workflows, … — is a
// flat sibling of this file), so this MUST stay generic/neutral rather than
// shaped like any one page — a dashboard-grid skeleton flashing on `/docs` or
// `/sign-in` would be worse than no skeleton at all. Routes with real, heavy,
// data-driven layouts (the dashboard home, workflows, a project, deployments,
// network, storage, observability) get their own accurately-shaped
// `loading.tsx` alongside their `page.tsx`, which takes precedence here.
export default function Loading() {
  return (
    <div className="flex flex-col gap-4 pb-24">
      <Skeleton className="h-7 w-56" />
      <Skeleton className="h-4 w-80 max-w-full" />
      <Skeleton className="mt-2 h-40 w-full" />
    </div>
  );
}
