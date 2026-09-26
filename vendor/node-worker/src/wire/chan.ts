// Named channels between the page and a program running in the worker.
//
// Everything else the page can say to a program goes through its stdio, and a control
// protocol multiplexed onto stdout is fine right up until the program prints something
// unexpected — which for a shell is not a hypothetical.
//
// Two shapes, because they answer different questions. `chan.open` hands the program a
// real `MessagePort` and gets out of the way: structured clone, transferables, and a
// protocol the program and the page agree on without this file's involvement. `chan.call`
// is the other half — a request to the host, by name, that a *synchronous* program can
// make, which a port can never serve because a parked worker will never read one.

/** One channel operation. */
export type ChanCall =
	/** Page → worker: here is the port for `name`. Reply attaches nothing. */
	| { op: "chan.open"; name: string }
	/**
	 * Worker → page: answer this, by name.
	 *
	 * Sync-capable, which is the point — a program blocked inside a synchronous call can
	 * still ask its host a question. Arguments and answer are whatever the two agreed on;
	 * bytes ride in `parts` rather than inside the JSON.
	 */
	| { op: "chan.call"; name: string; args?: unknown };

export interface ChanResults {
	"chan.open": null;
	"chan.call": unknown;
}

export type ChanOpName = ChanCall["op"];
export type ChanResult<K extends ChanOpName> = ChanResults[K];
