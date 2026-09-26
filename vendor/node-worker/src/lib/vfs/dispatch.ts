// Frame in, frame out. The one entry point both transports call.
//
// The synchronous path (a blocking XHR relayed by the service worker) and the asynchronous
// path (a postMessage) both land here, on the same bytes, through the same dispatch table.
// That is deliberate: the failure mode this boundary is most exposed to is a bug that
// exists on one transport and not the other, and there is nothing to diverge if there is
// only one implementation.
//
// It is also the seam that keeps the host VFS movable. Nothing here knows whether the caller
// is a service worker, a window, or a dedicated worker the page owns — so relaying frames to
// a VFS host worker later (which is where OPFS is fastest, since `createSyncAccessHandle` is
// worker-only) is a change to the plumbing above, not to this file.

import { toWireError } from "../../wire/error";
import { recall, remember, type ReplayCache } from "../../wire/replay";
import { decodeFrame } from "../../wire/frame";
import { primaryParts } from "../../wire/pack";
import { KIND_FS } from "../../wire/kinds";
import { encodeReply, type DispatchResult } from "../../wire/router";
import type { PuterFsEvent } from "../../wire/events";
import type { VfsCall, VfsRequest } from "../../wire/fs";
import type { WireReply } from "../../wire/message";
import { parseOpenFlags } from "../../vfs/flags";
import type { MountTable } from "./mounts";
import type { Facade } from "./facade";
import type { HandleRegistry } from "./handles";

/**
 * Ops whose answer depends on a file's current contents, size or mtime, addressed by path.
 *
 * `open` is in the set because it stats the file to seed the new handle's size, and a second fd
 * onto a file with a dirty first fd would otherwise start from the stale length. `readdir` is not:
 * its path is a directory, and no handle is ever open on one.
 */
const READS_THROUGH_PATH: ReadonlySet<string> = new Set([
	"stat",
	"access",
	"exists",
	"readFile",
	"readRange",
	"copyFile",
	"rename",
	"truncate",
	"cp",
	"open",
]);
import * as ops from "./ops";

export interface DispatchDeps {
	fs: Facade;
	table: MountTable;
	/** Open files. Lives here rather than in the worker; see ./handles.ts. */
	handles: HandleRegistry;
	sid: string;
	proto: number;
	/**
	 * Watch events produced *by this call*, drained into its reply.
	 *
	 * They ride the reply rather than being pushed separately for two reasons: it preserves
	 * the "emitted the moment the call succeeded" timing that makes write-then-observe feel
	 * immediate, and — the load-bearing half — it works while the worker is parked inside a
	 * synchronous call, because the event arrives on the response the worker is already
	 * waiting for. A pushed event would sit in the worker's message queue until the blocking
	 * XHR returned.
	 */
	drainEvents(): PuterFsEvent[];
	/** Resolver-cache invalidations this call implies. See ./virtual.ts. */
	drainInvalidations(): { paths?: string[]; subtrees?: string[] } | undefined;
	/** Backend call counts to report back, when the worker asked for them. */
	drainApiCalls?(): Record<string, number> | undefined;
	/** Streams for `fs.openRead`. Separate entries because an fd may hold unflushed bytes. */
	openRead(
		path: string,
		range: { start: number; end?: number }
	): Promise<{ stream: ReadableStream<Uint8Array>; size?: number }>;
	openReadFd(
		fd: number,
		range: { start: number; end?: number }
	): Promise<{ stream: ReadableStream<Uint8Array>; size?: number }>;
	/** The reply record, one window per session, for the exactly-once retry. See `createReplayCache`. */
	replies: ReplayCache;
}

/**
 * The result of one op: a value for the header, any bytes for the payload, and any handle
 * that has to be transferred rather than encoded.
 *
 * `transfer` is only ever `fs.openRead`'s stream. It is what makes that op async-only, and
 * it is also why that op is never remembered in the replay record below: a transferred
 * stream is consumed once, so replaying its reply would hand over a handle that is already
 * gone.
 */
type Answer = {
	value: unknown;
	parts?: Uint8Array[];
	transfer?: Transferable[];
};

// The exactly-once machinery moved to ../../wire/replay.ts when a second sync-capable kind
// needed it. Re-exported so this module's callers — ./index.ts holds the cache and forgets a
// session on teardown — keep importing it from the dispatcher that uses it.
export {
	createReplayCache,
	forgetReplays,
	type ReplayCache,
} from "../../wire/replay";

export async function handleFrame(
	deps: DispatchDeps,
	frame: ArrayBuffer | Uint8Array
): Promise<DispatchResult> {
	let request: VfsRequest;
	let parts: Uint8Array[];
	try {
		const decoded = decodeFrame<VfsRequest>(frame);
		request = decoded.header;
		// Sideband bytes are not this call's. `fdWritev` writes every part it is given, so
		// letting a piggybacked stdout chunk through here would write it into the file.
		parts = primaryParts(decoded.header, decoded.parts);
	} catch (err) {
		// A frame we cannot even parse has no seq to echo. Answer with one anyway so the
		// worker gets a node-shaped error rather than a decode failure of its own.
		return {
			frame: reply({
				seq: 0,
				result: { ok: false, error: toWireError(err, "read") },
			}),
		};
	}

	const seq = request.seq;

	// A repeat means the worker retried after a transport failure. Answer from the record rather
	// than running the operation again — the whole point of the retry being safe.
	const already = recall(deps.replies, deps.sid, seq);
	if (already) return { frame: already };

	let answer: Answer;
	try {
		answer = await perform(deps, request.call, parts);
	} catch (err) {
		const failed = reply(
			{
				seq,
				result: {
					ok: false,
					error: toWireError(err, ctxOf(request.call)?.syscall),
				},
			},
			deps
		);
		remember(deps.replies, deps.sid, seq, failed);
		return { frame: failed };
	}

	const ok = reply(
		{ seq, result: { ok: true, value: answer.value } },
		deps,
		answer.parts
	);
	// A reply carrying a handle is not replayable: the handle is transferred, so the record
	// would hand a second caller a stream that has already been detached. The retry it exists
	// for cannot happen anyway — the op is async-only, and only the sync path retries.
	if (!answer.transfer) remember(deps.replies, deps.sid, seq, ok);
	return { frame: ok, transfer: answer.transfer };
}

function ctxOf(
	call: VfsCall
): { syscall: string; reportPath: string } | undefined {
	return "ctx" in call ? call.ctx : undefined;
}

// The sidebands are drained here rather than in ../../wire/router.ts because only this
// side knows what is pending: the router builds the message, the filesystem decides what
// rides along on it.
function reply(
	body: WireReply,
	deps?: DispatchDeps,
	parts?: Uint8Array[]
): Uint8Array {
	return encodeReply(
		KIND_FS,
		body,
		parts,
		deps && {
			events: deps.drainEvents(),
			invalidate: deps.drainInvalidations(),
			apiCalls: deps.drainApiCalls?.(),
		}
	);
}

/** No bytes, just a value. */
const v = (value: unknown): Answer => ({ value });
/** Bytes in the payload; the header's value is null. */
const b = (bytes: Uint8Array): Answer => ({ value: null, parts: [bytes] });

async function perform(
	deps: DispatchDeps,
	call: VfsCall,
	parts: Uint8Array[]
): Promise<Answer> {
	const { fs } = deps;

	// An open fd buffers its writes host-side (see HandleRegistry#flushPath). Anything that then
	// reads the same file *by path* has to see them, so those ops flush first. Listed explicitly
	// rather than inferred from "has a path" because the write-side ops must NOT appear here: a
	// path-level `writeFile` racing a dirty fd is last-writer-wins either way, and flushing first
	// would just make the fd's stale buffer the winner.
	if (READS_THROUGH_PATH.has(call.op)) {
		const target =
			(call as { path?: string; from?: string }).path ??
			(call as { from?: string }).from;
		if (typeof target === "string") await deps.handles.flushPath(target);
	}

	switch (call.op) {
		// Answered without touching the filesystem: this is the startup probe that verifies
		// the service worker is really intercepting, and it echoes the session id back so a
		// misrouted request is caught rather than silently served.
		case "probe":
			return v({ proto: deps.proto, sid: deps.sid });
		case "mounts":
			return v(deps.table.snapshot());

		case "stat":
			return v(await fs.stat(call.ctx, call.path));
		case "access":
			return v((await fs.stat(call.ctx, call.path), null));
		case "exists":
			return v(await ops.exists(fs, call.ctx, call.path));
		case "statfs":
			return v(await fs.statfs(call.ctx, call.path));
		case "readdir":
			return v(await fs.readdir(call.ctx, call.path, call.opts));
		case "readFile":
			return b(await fs.readFile(call.ctx, call.path));
		case "readRange":
			return b(
				await fs.readRange(call.ctx, call.path, call.offset, call.length)
			);
		case "writeFile":
			await fs.writeFile(call.ctx, call.path, payload(parts));
			return v(null);
		case "append":
			await ops.append(fs, call.ctx, call.path, payload(parts));
			return v(null);
		case "mkdir":
			return v(
				await fs.mkdir(call.ctx, call.path, { recursive: call.recursive })
			);
		case "rm":
			await fs.rm(call.ctx, call.path, {
				recursive: call.recursive,
				force: call.force,
			});
			return v(null);
		case "rmrf":
			await ops.rmrf(fs, call.ctx, call.path, call.force);
			return v(null);
		case "rename":
			await fs.rename(call.ctx, call.from, call.to);
			return v(null);
		case "copyFile":
			await fs.copyFile(call.ctx, call.from, call.to, {
				overwrite: call.overwrite,
			});
			return v(null);
		case "utimes":
			return v(
				await ops.utimes(fs, call.ctx, call.path, call.atimeMs, call.mtimeMs)
			);
		case "truncate":
			await ops.truncate(fs, call.ctx, call.path, call.length);
			return v(null);
		case "mkdtemp":
			return v(await ops.mkdtemp(fs, call.ctx, call.prefix));
		case "cp":
			await ops.cp(fs, call.ctx, call.from, call.to, {
				recursive: call.recursive,
				force: call.force,
				errorOnExist: call.errorOnExist,
			});
			return v(null);

		// ------------------------------------------------------------ the fd family
		case "open": {
			// The read strategy is fixed for the fd's lifetime from the mount's capability,
			// which is why this is resolved here rather than guessed by the handle.
			const mount = deps.table.resolve(call.path).mount;
			const { fd } = await deps.handles.open(
				call.path,
				parseOpenFlags(call.flags),
				!!mount.provider.readRange,
				// Whose fd this is. One `NodeVfs` may back several workers, and `closeSession`
				// has to be able to drop this one's descriptors without touching theirs.
				deps.sid
			);
			return v({ fd });
		}
		case "close":
			await deps.handles.close(call.fd);
			return v(null);
		case "fdRead":
			return b(
				await deps.handles.get(call.fd, "read").read(call.length, call.position)
			);
		case "fdReadv": {
			const chunks = await deps.handles
				.get(call.fd, "readv")
				.readv(call.lengths, call.position);
			return { value: null, parts: chunks };
		}
		case "fdWrite":
			return v(
				await deps.handles
					.get(call.fd, "write")
					.write(payload(parts), call.position)
			);
		case "fdWritev":
			return v(
				await deps.handles.get(call.fd, "writev").writev(parts, call.position)
			);
		case "fdReadFile":
			return b(await deps.handles.get(call.fd, "read").readFile());
		case "fdWriteFile":
			await deps.handles.get(call.fd, "write").writeFile(payload(parts));
			return v(null);
		case "fdAppend":
			await deps.handles.get(call.fd, "write").appendFile(payload(parts));
			return v(null);
		case "fdStat":
			return v(await deps.handles.get(call.fd, "fstat").stat());
		case "fdTruncate":
			await deps.handles.get(call.fd, "ftruncate").truncate(call.length);
			return v(null);
		case "fdSync":
			await deps.handles.get(call.fd, "fsync").sync();
			return v(null);
		case "fdUtimes":
			return v(
				await deps.handles
					.get(call.fd, "futimes")
					.utimes(call.atimeMs, call.mtimeMs)
			);
		case "fs.openRead": {
			// The stream is the answer, and it is transferred rather than encoded — see the
			// note on `Answer.transfer`. `size` is whatever the host already knew, so a
			// consumer that wants a length does not have to stat separately.
			const opened =
				call.fd !== undefined
					? await deps.openReadFd(call.fd, {
							start: call.start ?? 0,
							end: call.end,
						})
					: await deps.openRead(call.path!, {
							start: call.start ?? 0,
							end: call.end,
						});
			return {
				value: { size: opened.size },
				transfer: [opened.stream as unknown as Transferable],
			};
		}

		case "fdFlushWrite": {
			// write + sync + close as one op, which is what `Utf8Stream.flushSync` wants and
			// what used to cost it five blocking round trips.
			const handle = deps.handles.get(call.fd, "write");
			if (parts.length) await handle.write(payload(parts), null);
			await handle.sync();
			await deps.handles.close(call.fd);
			return v(null);
		}
	}
	// Exhaustive over VfsCall; a new op that forgets a branch fails to compile.
	const never: never = call;
	throw new Error(`unknown vfs op: ${JSON.stringify(never)}`);
}

function payload(parts: Uint8Array[]): Uint8Array {
	return parts[0] ?? new Uint8Array(0);
}
