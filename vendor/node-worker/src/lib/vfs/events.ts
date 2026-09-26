// Watch events synthesized from a mutation this filesystem just performed.
//
// puterfs echoes every write back over its socket.io feed, so watchers would eventually
// see our own changes anyway — but only after a round trip, and only while the socket is
// up. Emitting the moment a call succeeds is what makes write-then-observe (chokidar's
// `awaitWriteFinish`, a dev server's HMR trigger) feel immediate. A memory or OPFS mount
// has no feed at all, so for those this is the *only* source of events.
//
// Passed to a provider rather than reached for as a module global, because the events
// have to arrive at the worker that caused them and the filesystem is per session. The
// transport carries them back on the reply frame to the very call that produced them —
// which preserves the timing above and, unlike a separate push, works while the worker
// is parked inside a synchronous call, because the event rides the response it is
// already waiting for.
//
// Echoes are deliberately NOT deduplicated against the socket feed. The local event is
// the fast, approximate signal (nothing on the write path stats first, so a creation may
// be reported as `updated`); the echo that follows carries the api's own classification.
// node's `fs.watch` is documented as coalescing and occasionally double-reporting, and
// every real consumer re-stats and dedupes anyway — so an extra event is cheap and a
// missing one is not.

import type { PuterFsEvent } from "../../wire/events";

export interface FsEvents {
	/**
	 * A file's contents changed. Used for writes even when the file was just created:
	 * nothing on the write path stats first, so `added` and `updated` cannot be told
	 * apart without an extra round trip.
	 */
	write(path: string): void;
	add(path: string, isDir?: boolean): void;
	remove(path: string, isDir?: boolean): void;
	move(oldPath: string, path: string, isDir?: boolean): void;
}

export function createFsEvents(emit: (event: PuterFsEvent) => void): FsEvents {
	return {
		write(path) {
			emit({ kind: "updated", path, isDir: false });
		},
		add(path, isDir = false) {
			emit({ kind: "added", path, isDir });
		},
		remove(path, isDir = false) {
			emit({ kind: "removed", path, isDir });
		},
		move(oldPath, path, isDir = false) {
			emit({ kind: "moved", path, oldPath, isDir });
		},
	};
}

/** For a provider built outside a session — tests, and the host reading its own mount. */
export const NO_EVENTS: FsEvents = {
	write() {},
	add() {},
	remove() {},
	move() {},
};
