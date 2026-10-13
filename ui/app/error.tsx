"use client";

import { useEffect } from "react";

/** Route-segment error boundary: a render/runtime exception shows its message with a retry (and is logged to the console) instead of unmounting the page to a blank
 *  screen. */
export default function Error({ error }: { error: Error & { digest?: string }; reset: () => void }) {
  useEffect(() => {
    console.error("[route error]", error);
  }, [error]);
  return (
    <div className="flex min-h-[50vh] flex-col items-center justify-center gap-3 px-6 text-center">
      <div className="text-lg font-semibold">Something went wrong on this page</div>
      <p className="max-w-md text-sm text-secondary">
        {error?.message || "An unexpected error occurred while rendering."}
      </p>
      <div className="mt-1 flex gap-2">
        <button
          onClick={() => window.location.reload()}
          className="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90"
        >
          Try again
        </button>
        <button
          // eslint-disable-next-line @next/next/no-location-assign-relative-destination
          onClick={() => (window.location.href = "/")}
          className="rounded-md border border-border px-3 py-1.5 text-sm text-secondary hover:bg-subtle"
        >
          Go home
        </button>
      </div>
    </div>
  );
}
