// The stdio op set.
//
// This is the kind the whole merge was for. `fd-table.ts` has always reserved 0, 1 and 2
// "leaving room for stdio" and nothing ever registered them, so `fs.readSync(0, …)` and
// `fs.writeSync(1, …)` threw EBADF — which breaks `readline-sync`, `prompt-sync`, and the
// `readFileSync(0)` idiom for reading piped input. Stdio lived on the structured-clone
// envelope, and that envelope could never be synchronous, so it could not be fixed without
// one format that serves both transports.
//
// Reads and writes are asymmetric on purpose, and it is worth being explicit about why.
//
// **A write does not block.** `io.write` appends to a worker-side buffer and returns the
// length, and the buffer leaves as a *sideband* on whatever message goes next — see
// ./pack.ts. Routing every `console.log` through its own blocking XHR would be far too
// chatty, and the obvious alternative is impossible rather than merely slow:
// `fs.writeFileSync` is a synchronous function, so it can never await acknowledgement of
// previously-queued writes. Piggybacking is the only arrangement that guarantees ordering
// at a synchronous boundary, because the pending bytes are *inside* the message that
// boundary sends.
//
// **A read does block**, and may block for as long as a person takes to answer a prompt.
// That is what the `SwProgress` heartbeat in ./sw.ts is for: the service worker's deadline
// is a liveness check, not a limit on how long an op may take.

export type StdioCall =
	/**
	 * Bytes for fd 1 or 2, in `parts[0]`.
	 *
	 * Normally a push rather than a call — nothing waits on it — but it is a request like
	 * any other when a caller does want the flush acknowledged, which `process.exit` does.
	 */
	| { op: "io.write"; fd: 1 | 2 }
	/**
	 * Read from fd 0.
	 *
	 * `blocking` distinguishes a prompt from a poll. A blocking read waits for input or for
	 * end-of-stream; a non-blocking one answers with whatever is buffered and reports
	 * `eof: false` with no bytes when that is nothing, which is what a non-blocking fd does.
	 */
	| { op: "io.read"; fd: 0; length: number; blocking: boolean }
	/** Everything buffered on 1 and 2 is through to the terminal. */
	| { op: "io.flush" };

export interface StdioResults {
	"io.write": null;
	/** Bytes in `parts[0]`. `eof` means the stream ended, not that this read was empty. */
	"io.read": { eof: boolean };
	"io.flush": null;
}

export type StdioOpName = StdioCall["op"];
export type StdioResult<K extends StdioOpName> = StdioResults[K];
