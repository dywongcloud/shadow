/** The overview shows at most this many open incidents (newest first). */
export const OPEN_INCIDENTS_LIMIT = 25;
export const OPEN_INCIDENTS_PATH = `/v1/incidents?status=open&limit=${OPEN_INCIDENTS_LIMIT}`;

/** The incidents page: open incidents, capped. */
export const OPEN_LIMIT = 100;
export const OPEN_PATH = `/v1/incidents?status=open&limit=${OPEN_LIMIT}`;

/** Resolved history is paged on demand. */
export const RESOLVED_PAGE = 25;
export function resolvedPath(offset: number): string {
  return `/v1/incidents?status=resolved&limit=${RESOLVED_PAGE}&offset=${offset}`;
}
