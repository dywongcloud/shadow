"use client";

import { useEffect } from "react";

/**
 * Route-segment error boundary. Any render/runtime exception in a page subtree
 * lands here instead of unmounting the tree to a BLANK screen (the failure mode
 * one user hit on /storage). Shows the message + a retry, and logs to the console
 * so the real cause is recoverable. Purely additive: never renders on success.
 */
export default function Error({ error, reset }: { error: Error & { digest?: string }; reset: () => void }) {
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
          onClick={reset}
          className="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90"
        >
          Try again
        </button>
        <button
          onClick={() => (window.location.href = "/")}
          className="rounded-md border border-border px-3 py-1.5 text-sm text-secondary hover:bg-subtle"
        >
          Go home
        </button>
      </div>
    </div>
  );
}
