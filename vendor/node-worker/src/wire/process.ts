// The process op set.
//
// The framing, the transports and the sequence numbering all come from ./frame.ts and
// ./message.ts — this is only the vocabulary, exactly as ./fs.ts is. A message says
// which dispatcher it belongs to in its `kind` field (`KIND_PROCESS`), which the host
// reads without parsing anything.

import type { ProcCtx, SpawnRequest } from "../process/provider";
import type { WireRequest } from "./message";

/** One process operation. Mirrors `VfsCall`. */
export type ProcessCall =
	/** Verifies a provider is registered at all, before anything depends on one. */
	| { op: "proc.probe" }
	| { op: "proc.spawn"; ctx: ProcCtx; request: SpawnRequest }
	| { op: "proc.poll"; ctx: ProcCtx; pid: number }
	/** Bytes in `parts[0]`. */
	| { op: "proc.write"; ctx: ProcCtx; pid: number }
	| { op: "proc.endStdin"; ctx: ProcCtx; pid: number }
	| { op: "proc.kill"; ctx: ProcCtx; pid: number; signal: string }
	/** `input` in `parts[0]`, when there is any. */
	| { op: "proc.spawnSync"; ctx: ProcCtx; request: SpawnRequest };

/** The process request header — {@link WireRequest} carrying a {@link ProcessCall}. */
export type ProcessRequest = WireRequest<ProcessCall>;

/**
 * What each op answers with.
 *
 * Bytes never travel inside the JSON: `poll` describes its events in the header and puts their
 * payloads in `parts`, in the same order, and `spawnSync` does the same for stdout and stderr.
 * That keeps a megabyte of program output out of `JSON.stringify` — the filesystem learned the
 * same lesson, which is why `readFile` answers this way too.
 */
export type ProcessResult<K extends ProcessCall["op"]> = K extends "proc.probe"
	? { provider: string }
	: K extends "proc.spawn"
		? { pid: number }
		: K extends "proc.poll"
			? { events: PolledEvent[] }
			: K extends "proc.spawnSync"
				? {
						status: number | null;
						signal: string | null;
						error?: { message: string; code?: string };
					}
				: void;

/** A `ProcEvent` with its bytes lifted out into the message's parts. */
export type PolledEvent =
	| { kind: "stdout" | "stderr" }
	| { kind: "exit"; status: number | null; signal: string | null };
