// Filesystem change events, as messages rather than as a port.
//
// These used to own a `MessagePort` with its own tagged union on it, and the port was the
// problem: a worker parked inside a blocking XHR never reads one. So a `fs.watch` saw
// nothing at all for the whole duration of a synchronous loop, and the runtime carried a
// *second*, unrelated delivery path — `WireReply.events` — precisely to cover the case the
// port could not.
//
// There is one path now. An event is a push: it rides the reply the worker is already
// waiting for, or goes on its own when the worker is idle. That is strictly better than
// the port on the axis that mattered, and it deletes the `FsEventsToWorker` /
// `FsEventsToPage` unions along with the message types that handed the port over.

/**
 * A puterfs mutation, normalized from the api's `item.*` socket.io events (or
 * synthesized locally by the worker's own fs calls).
 *
 * Kinds map onto puter's wire events; `node:fs`'s rename/change distinction is applied
 * later, in worker/node/fs/watch.ts, because it depends on what the watcher is watching.
 */
export interface PuterFsEvent {
	kind: "added" | "updated" | "removed" | "moved";
	/** Absolute puterfs path of the entry the event is about. */
	path: string;
	isDir: boolean;
	/** `moved` only: where the entry came from. */
	oldPath?: string;
	/**
	 * `removed` only: the parent survived and only its children were dropped
	 * (how the api reports emptying Trash).
	 */
	descendantsOnly?: boolean;
	/**
	 * The entry's stable id, when the source knew one.
	 *
	 * Present on anything the socket delivered, absent on a locally synthesized
	 * event (nothing on the write path stats first, so there is no uid to report).
	 * The cache uses it to catch a *third-party* rename: the api answers `POST
	 * /rename` with `item.updated` naming only the new path, so the entry at the
	 * old one would otherwise stay cached forever. Same uid at a different path
	 * means the old path is gone.
	 */
	uid?: string;
}

/** One event operation. */
export type EventsCall =
	/** Worker → page: start the feed. Answers with the state the feed starts in. */
	| { op: "ev.subscribe"; token?: string; apiOrigin: string }
	/** Worker → page: stop it. */
	| { op: "ev.close" }
	/** Page → worker: one mutation. */
	| { op: "ev.fs"; event: PuterFsEvent }
	/**
	 * Page → worker: whether anything is watching on the host's behalf.
	 *
	 * Lets a watcher tell "nothing has changed" from "we're not listening right now" — the
	 * latter is when `watchFile`'s poll fallback earns its keep. `polling` is the second
	 * half of that: the change counter answering is not as good as a live socket, but it is
	 * enough that a `watchFile` need not run its own stat loop. Only when *neither* is true
	 * is there nothing at all, which is the one case node's interval has to cover.
	 */
	| { op: "ev.state"; connected: boolean; polling: boolean }
	/**
	 * Page → worker: something under this user changed, and we were not told what.
	 *
	 * The coarse half of the poll fallback. It carries no path because the source has none —
	 * see the poller in lib/fsevents.ts. A consumer answers it by revalidating whatever it
	 * holds, not by assuming any particular path moved.
	 */
	| { op: "ev.stale"; timestamp: number }
	| { op: "ev.error"; message: string; fatal: boolean };

export interface EventsResults {
	"ev.subscribe": { connected: boolean; polling: boolean };
	"ev.close": null;
	"ev.fs": null;
	"ev.state": null;
	"ev.stale": null;
	"ev.error": null;
}

export type EventsOpName = EventsCall["op"];
export type EventsResult<K extends EventsOpName> = EventsResults[K];
