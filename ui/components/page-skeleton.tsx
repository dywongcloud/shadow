/** Generic responsive route skeleton for each route's `loading.tsx`: a title and subtitle bar over a card grid (1/2/3 columns on phone/tablet/desktop), aria-hidden and sized
 *  to the real content box so swapping the page in shifts the layout as little as possible. */
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
