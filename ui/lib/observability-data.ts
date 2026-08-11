import "server-only";

// Server-side counterpart to lib/api.ts's apiGet -- same role for
// Observability/Speed Insights that lib/ops-data.ts's fetchOpsServer plays for
// the admin pages: one place that resolves the tenant and fetches+validates
// the backend response, shared by the two cached route handlers
// (app/api/observability/{summary,speed-insights}/route.ts) AND the
// Observability page's own Server Component prefetch, so all three agree on
// exactly what "a valid response" means instead of drifting.
import { backend } from "@/lib/gitops-server";
import { resolveEntity } from "@/lib/composio";
import type { Metrics, RumSummary } from "@/lib/api";

/** Resolve the tenant server-side -- never trust a client-supplied header
 *  alone; it is honored only as an override when explicitly passed. */
export async function resolveTeam(headerTeam?: string | null): Promise<string> {
  return headerTeam || (await resolveEntity()) || "personal";
}

/** Thrown by the fetch helpers below, carrying the exact HTTP status the
 *  caller should surface -- a route handler reads `.status` directly instead
 *  of parsing a message string, and a Server Component prefetch can just
 *  catch-and-null regardless of which status it was. */
export class BackendFetchError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

async function fetchAndValidate<T>(path: string, team: string, what: string): Promise<T> {
  let r: Response;
  try {
    r = await backend(path, team);
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new BackendFetchError(`${what} backend unreachable: ${msg}`, 503);
  }
  if (!r.ok) {
    // 401/403 pass through as-is (the caller genuinely isn't authorized);
    // every other non-2xx is an upstream fault, reported as 502.
    const status = r.status === 401 || r.status === 403 ? r.status : 502;
    throw new BackendFetchError(`${what} backend returned ${r.status}`, status);
  }
  const data = await r.json().catch(() => null);
  if (data == null) throw new BackendFetchError(`${what} backend returned a malformed body`, 502);
  return data as T;
}

/** Fetch + validate GET /v1/metrics. Throws BackendFetchError on any failure
 *  (unreachable, non-2xx, malformed body) -- callers decide how to surface
 *  that: the route handler turns it into an HTTP response with the same
 *  status, the Server Component prefetch swallows it into a null (a summary
 *  is a convenience over the live poll, which keeps running either way; a
 *  failed prefetch must never blank the page). */
export async function fetchMetricsSummary(team: string, minutes: number): Promise<Metrics> {
  return fetchAndValidate<Metrics>(`/v1/metrics?minutes=${minutes}`, team, "metrics");
}

/** Fetch + validate GET /v1/speed-insights. Same contract as
 *  fetchMetricsSummary above. */
export async function fetchSpeedInsightsSummary(
  team: string,
  minutes: number,
  device?: "desktop" | "mobile"
): Promise<RumSummary> {
  const q = device ? `&device=${device}` : "";
  return fetchAndValidate<RumSummary>(`/v1/speed-insights?minutes=${minutes}${q}`, team, "speed-insights");
}
