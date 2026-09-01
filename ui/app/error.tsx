"use client";

import { useEffect } from "react";

/**
 * Route-segment error boundary. Any render/runtime exception in a page subtree
 * lands here instead of unmounting the tree to a BLANK screen (the failure mode
 * one user hit on /storage). Shows the message + a retry, and logs to the console
 * so the real cause is recoverable. Purely additive: never renders on success.
 */
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
        {/* Full reload, not reset(): reset() re-renders the SAME page, so a crash
            caused by a stale cached bundle (SWR-stale HTML referencing an old
            immutable chunk in the browser's disk cache) can never recover through
            it. A hard reload refetches the HTML, which past the SWR window
            references the current fixed chunks. */}
        <button
          onClick={() => window.location.reload()}
          className="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90"
        >
          Try again
        </button>
        <button
          // Hard navigation, not useRouter().push(): same reasoning as the reload
          // button above — client-side router navigation reuses the exact React
          // tree/bundle that just crashed, so it can't reliably recover.
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
