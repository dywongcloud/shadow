// What a process host has to implement, and nothing about how it does it.
//
// The mirror of ../vfs/provider.ts, and deliberately the same shape: something on the *host*
// side answers, and the worker reaches it over the transport it already has. That matters more
// here than it looks. A shell is a program that runs for a while, writes as it goes and can be
// waited on — none of which a worker can host for *another* worker, because only a page can
// create a `NodeWorker` (a `DedicatedWorkerGlobalScope` has no `navigator.serviceWorker`, so no
// synchronous filesystem and no module resolver on the other side).
//
// Keeping it here also keeps the worker bundle out of it: the implementation is a plugin the
// embedder registers, the way a `VfsProvider` is, rather than several megabytes of shell that
// every worker pays for whether or not it ever spawns anything.

/** Per-call context, threaded across the boundary. Matches `WireCtx` in ../vfs/entry.ts. */
export interface ProcCtx {
	/** node's syscall name, for the error's `syscall` field: "spawn", "kill", … */
	readonly syscall: string;
}

export interface SpawnRequest {
	/** argv[0] as the caller spelled it — resolution against PATH belongs to the provider. */
	file: string;
	args: string[];
	cwd?: string;
	env?: Record<string, string>;
	/**
	 * Per-descriptor disposition, index 0..2. "pipe" is the only one the worker can act on;
	 * "inherit" and "ignore" are the host's to interpret, since it owns the terminal.
	 */
	stdio?: ("pipe" | "inherit" | "ignore")[];
}

/** What a finished process leaves behind. `signal` and `status` are mutually exclusive. */
export interface ExitStatus {
	status: number | null;
	signal: string | null;
}

/**
 * One thing that happened to a running process.
 *
 * Output arrives as an event rather than being pushed, because the transport underneath is
 * strictly request-and-reply: the worker keeps one `poll` outstanding per child and the host
 * answers it when there is something to say. That is what lets the *same* protocol carry a
 * streaming child and a blocking `spawnSync`.
 */
export type ProcEvent =
	| { kind: "stdout" | "stderr"; bytes: Uint8Array }
	| ({ kind: "exit" } & ExitStatus);

export interface SpawnSyncResult extends ExitStatus {
	stdout: Uint8Array;
	stderr: Uint8Array;
	/** Set when the process could not be run *at all*, which is not the same as a failure. */
	error?: { message: string; code?: string };
}

/**
 * A host that can run programs.
 *
 * Every method may reject; a rejection is turned into a node-shaped error on the worker's side,
 * so `err.code` survives the crossing (see ../vfs/errno.ts, which does the same for the
 * filesystem).
 */
export interface ProcessProvider {
	/** For diagnostics. */
	readonly name: string;

	/** Start a process and return the id the worker will refer to it by. */
	spawn(ctx: ProcCtx, request: SpawnRequest): Promise<{ pid: number }>;

	/**
	 * Whatever has happened since the last call, waiting until something has.
	 *
	 * Resolving empty is allowed and means "nothing yet, ask again" — a provider that cannot
	 * wait can poll instead, at the cost of a round trip per turn.
	 */
	poll(ctx: ProcCtx, pid: number): Promise<ProcEvent[]>;

	/** Write to the child's stdin. */
	write(ctx: ProcCtx, pid: number, bytes: Uint8Array): Promise<void>;

	/** No more stdin. A child reading to EOF is waiting for exactly this. */
	endStdin(ctx: ProcCtx, pid: number): Promise<void>;

	kill(ctx: ProcCtx, pid: number, signal: string): Promise<void>;

	/**
	 * Run to completion and answer once.
	 *
	 * This is the call that decides the architecture. `child_process.spawnSync` must answer
	 * without returning to the *worker's* event loop, and it does: the worker blocks in an
	 * `XMLHttpRequest` the service worker relays, exactly as `fs.readFileSync` does, while the
	 * host is free to run the program asynchronously and answer when it finishes. So a provider
	 * may take as long as it likes here and may use its own event loop freely — the only thing
	 * that is blocked is the caller.
	 */
	spawnSync(
		ctx: ProcCtx,
		request: SpawnRequest & { input?: Uint8Array }
	): Promise<SpawnSyncResult>;
}
