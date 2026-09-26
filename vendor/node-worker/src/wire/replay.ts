// The reply record that makes a synchronous retry exactly-once.
//
// A synchronous send retries on a transport failure with the **same** `seq` (see
// `SYNC_RETRIES` in ../worker/node/fs/transport.ts). That is only safe because the host
// answers a repeat from this record instead of running the operation a second time —
// without it a retried `append` appends twice, and a retried `proc.spawnSync` runs the
// program again, which with a real shell behind it is a command executed two or three
// times for one call.
//
// This lives in `wire/` rather than in the filesystem's dispatcher because every
// sync-capable kind needs it, not just `fs`. `KIND_PROCESS` and `KIND_CHAN` are both in
// `SYNC_CAPABLE` (./kinds.ts) and both carry side effects a caller would hate to see twice.

/**
 * Frames already answered, keyed by session and then by sequence number.
 *
 * **Per session, not global**, and a session is a *worker* rather than a filesystem. Sequence
 * numbers are minted per worker and start from 1, so a record shared between two workers answers
 * one with the other's reply. That used to be the same thing — one `NodeVfs` backed one worker —
 * but a vfs may back several, and then the two diverge: every worker on it opens at seq 1 and
 * collides with its predecessor from the first request onward. The symptom is not a failure but
 * *wrong data*, indistinguishable from a correct answer, which is the worst kind a filesystem has.
 *
 * Bounded, and small on purpose: it only has to cover an immediate retry, not history. The window
 * is per session too, so a busy worker cannot evict a quiet one's entries out from under it.
 */
const REPLAY_WINDOW = 64;

export type ReplayCache = Map<string, Map<number, Uint8Array>>;

export function createReplayCache(): ReplayCache {
	return new Map();
}

export function recall(
	cache: ReplayCache,
	sid: string,
	seq: number
): Uint8Array | undefined {
	return cache.get(sid)?.get(seq);
}

export function remember(
	cache: ReplayCache,
	sid: string,
	seq: number,
	frame: Uint8Array
) {
	let window = cache.get(sid);
	if (!window) {
		window = new Map();
		cache.set(sid, window);
	}
	window.set(seq, frame);
	// Insertion-ordered, so the oldest key is the first one.
	for (const key of window.keys()) {
		if (window.size <= REPLAY_WINDOW) break;
		window.delete(key);
	}
}

/** Drop a session's record once its worker is gone, so the map does not grow with the session count. */
export function forgetReplays(cache: ReplayCache, sid: string): void {
	cache.delete(sid);
}
