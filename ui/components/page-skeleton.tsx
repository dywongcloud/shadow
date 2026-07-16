/**
 * Generic responsive route skeleton, rendered by each route's `loading.tsx`
 * during navigation / data-segment loading (Next.js Suspense boundary). It gives
 * an instant, layout-stable placeholder on EVERY device — a title + subtitle bar
 * over a responsive card grid (1 col on phones, 2 on tablets, 3 on desktop) — so
 * route transitions never show a blank frame on mobile. Purely presentational
 * (aria-hidden, no data), and sized to roughly the real content box so swapping
 * the page in causes minimal layout shift.
 */
export function PageSkeleton() {
  return (
    <div className="animate-pulse space-y-6" aria-hidden="true">
      <div className="space-y-3">
        <div className="h-7 w-48 max-w-[70%] rounded-md bg-subtle" />
        <div className="h-4 w-80 max-w-[85%] rounded bg-subtle/60" />
      </div>
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {Array.from({ length: 6 }).map((_, i) => (
          <div key={i} className="h-32 rounded-xl border border-border bg-subtle/40" />
        ))}
      </div>
    </div>
  );
}
