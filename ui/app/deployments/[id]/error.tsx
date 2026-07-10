"use client";

import { useEffect } from "react";
import Link from "next/link";

/**
 * Route-specific error boundary for a single deployment's detail page. A render
 * exception here (a malformed deployment record, a build endpoint that misbehaves,
 * a helper that throws on an odd field) lands on THIS boundary instead of blanking
 * the route — the deployments list and the rest of the app stay intact, and the
 * user gets the message + a retry rather than a white screen. Purely additive:
 * never renders on success.
 */
export default function DeploymentError({ error }: { error: Error & { digest?: string }; reset: () => void }) {
  useEffect(() => {
    console.error("[deployment detail error]", error);
  }, [error]);
  return (
    <div className="mx-auto flex min-h-[50vh] max-w-5xl flex-col items-center justify-center gap-3 px-6 text-center">
      <div className="text-lg font-semibold">This deployment couldn&apos;t be displayed</div>
      <p className="max-w-md text-sm text-secondary">
        {error?.message || "An unexpected error occurred while rendering this deployment."}
      </p>
      <div className="mt-1 flex gap-2">
        {/* Hard reload, not reset(): recovers stale-cached-bundle crashes too
            (see app/error.tsx). */}
        <button
          onClick={() => window.location.reload()}
          className="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90"
        >
          Try again
        </button>
        <Link
          href="/deployments"
          className="rounded-md border border-border px-3 py-1.5 text-sm text-secondary hover:bg-subtle"
        >
          All deployments
        </Link>
      </div>
    </div>
  );
}
