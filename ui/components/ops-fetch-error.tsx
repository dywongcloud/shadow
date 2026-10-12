"use client";

import { Card } from "@/components/ui";

/**
 * The one failure surface for every `/admin` ops page.
 *
 * An operator page must never present "nothing here" when the truth is "I could
 * not look" -- see `fetchOpsResult` in lib/ops-data.ts for the case that
 * motivated this. Rendered whenever the server prefetch OR the client's own poll
 * reports a failure; returns nothing while there is none, so callers can render
 * it unconditionally.
 */
export function OpsFetchError({
  error,
  label,
}: {
  error: string | null | undefined;
  label: string;
}) {
  if (!error) return null;
  return (
    <Card className="mt-4 border-red-500/40 p-4 text-sm text-red-600 dark:text-red-400">
      Could not load {label}: {error}
    </Card>
  );
}
