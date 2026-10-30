import "server-only";

import { backend } from "@/lib/gitops-server";
import { resolveEntity } from "@/lib/composio";
import type { Metrics, RumSummary } from "@/lib/api";

export async function resolveTeam(headerTeam?: string | null): Promise<string> {
  return headerTeam || (await resolveEntity()) || "personal";
}

/** Carries the HTTP status the caller should surface: route handlers read `.status`, Server Component prefetches just catch and fall back to null. */
export class BackendFetchError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

/** The single validity rule shared by the observability route handlers and the page prefetch: unreachable -> 503, 401/403 pass through, an anonymous-tenant
 *  answer (the node did not accept the session) -> 401, any other non-2xx or an unparseable body -> 502. */
async function fetchAndValidate<T>(path: string, team: string, what: string, authToken: string): Promise<T> {
  let r: Response;
  try {
    r = await backend(path, team, authToken);
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new BackendFetchError(`${what} backend unreachable: ${msg}`, 503);
  }
  if (r.headers.get("x-hive-anon") === "1") throw new BackendFetchError(`${what} backend did not accept the session`, 401);
  if (!r.ok) {
    const callerNotAuthorized = r.status === 401 || r.status === 403;
    const status = callerNotAuthorized ? r.status : 502;
    throw new BackendFetchError(`${what} backend returned ${r.status}`, status);
  }
  const data = await r.json().catch(() => null);
  if (data == null) throw new BackendFetchError(`${what} backend returned a malformed body`, 502);
  return data as T;
}

/** Fetch + validate GET /v1/metrics; throws BackendFetchError on any failure. The route handler surfaces it with the same status; the page prefetch swallows it
 *  into null (a summary is a convenience over the live poll, so a failed prefetch must never blank the page). */
export async function fetchMetricsSummary(team: string, minutes: number, authToken: string): Promise<Metrics> {
  return fetchAndValidate<Metrics>(`/v1/metrics?minutes=${minutes}`, team, "metrics", authToken);
}

/** Fetch + validate GET /v1/speed-insights (same contract as `fetchMetricsSummary`). */
export async function fetchSpeedInsightsSummary(
  team: string,
  minutes: number,
  authToken: string,
  device?: "desktop" | "mobile"
): Promise<RumSummary> {
  const q = device ? `&device=${device}` : "";
  return fetchAndValidate<RumSummary>(`/v1/speed-insights?minutes=${minutes}${q}`, team, "speed-insights", authToken);
}
