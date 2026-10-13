"use client";

import { Card } from "@/components/ui";

/** The one failure surface for every `/admin` ops page: an operator page must never show "nothing here" when the truth is "I could not look" (see `fetchOpsResult` in
 *  lib/ops-data.ts). Rendered when the server prefetch OR the client's own poll reports a failure and nothing otherwise, so callers render it unconditionally. */
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
