// The worker's end of the message port.
//
// This replaces ./conn.ts, which was the `{type, to, reply}` envelope: an inflight map
// keyed by a 16-character random string, a handler that packed thrown errors into a
// `{type:"error"}` reply, and a `self.onmessage` that told replies from requests by
// reading a `to` field. All four of those exist once now, in ../wire/endpoint.ts, and
// both sides use the same one — correlation is the message's own `seq`, and an error is
// the same in-band `WireError` every other kind reports.
//
// A leaf module by construction: it imports nothing but `../wire/*`, so the filesystem
// subgraph can depend on it without widening the module-init cycle ./node/fs/lazy-base.ts
// warns about.

import { PortEndpoint, type CallOptions } from "../wire/endpoint";
import { fromWireError } from "../wire/error";
import type { PortEnvelope, WireReply } from "../wire/message";

/**
 * The one endpoint. Every kind rides it, in both directions.
 *
 * Module-scope construction is deliberate and safe: it allocates a `Map` and nothing else
 * — no globals are read, no port is touched until {@link bootstrap} attaches one.
 */
export const wire = new PortEndpoint();

/** One answered call: the value, its bytes, and any handles that came with it. */
export interface Called<R> {
	value: R;
	parts: Uint8Array[];
	attachments: readonly unknown[];
}

/**
 * Ask the page something and unwrap the answer.
 *
 * The in-band failure is rethrown as the error it describes, with `code`, `errno`,
 * `syscall` and `path` intact — which is the whole reason `WireError` exists, and the
 * thing the old `{error: {message}}` shape on the filesystem channel silently dropped.
 */
export async function call<R>(
	kind: number,
	c: unknown,
	opts?: CallOptions
): Promise<Called<R>> {
	const { decoded, attachments } = await wire.call(kind, c, opts);
	const header = decoded.header as WireReply;
	if (!header.result.ok) throw fromWireError(header.result.error);
	return { value: header.result.value as R, parts: decoded.parts, attachments };
}

/**
 * Take the first raw `postMessage` and become port-driven.
 *
 * The bootstrap is the one message that cannot arrive over the port, because it is what
 * delivers the port. By contract it carries the port as its first attachment; everything
 * after it — including this message's own reply — goes over that port, and
 * `self.onmessage` is dropped so there is exactly one way in.
 */
export function bootstrap(): void {
	self.onmessage = (e: MessageEvent) => {
		const envelope = e.data as PortEnvelope | undefined;
		if (!envelope?.f) return;
		const attachments = [...(envelope.a ?? [])];
		const port = attachments.shift() as MessagePort | undefined;
		if (!port) {
			throw new Error(
				"node-worker: the bootstrap message carried no port — the host and worker builds disagree"
			);
		}
		self.onmessage = null;
		wire.attach(port);
		// Routed like any other message, so `ctl.init` is not a special case beyond how it
		// arrived. Its reply goes back over the port that came with it.
		void wire.deliver(envelope.f, attachments);
	};
}
