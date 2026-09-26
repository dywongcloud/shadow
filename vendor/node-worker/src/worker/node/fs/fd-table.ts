// Global numeric file-descriptor registry.
//
// Node hands out integer fds from a single process-wide space shared by the sync,
// callback, and promise APIs, and an fd from any of them works with all of them.
// One counter and one table, holding one kind of handle — the sync and async
// families used to store different classes here and reject each other's fds with
// EBADF, which node never does.
//
// Numbers are minted by the *host*, which owns the handles (src/lib/vfs/handles.ts) — this
// side only maps them back to the wrapper that node's fd families hand out. The host starts at
// 10, and that is load-bearing beyond leaving room for stdio: the esbuild-wasm shim
// (node-worker-test/src/shims/esbuild-wasm.cjs) bridges Go's filesystem calls by dispatching on
// the fd number — 0/1/2 are its own stdio protocol and anything >= 10 is forwarded to us.
// Lowering it would break vite's dependency optimizer.
//
// This module intentionally imports nothing, so it can be a dependency of both the handle and
// everything that looks handles up without creating a cycle. Hence the structural type rather
// than importing FileHandle.

/** The shape the table guarantees. The only implementation is `FileHandle`. */
export interface HandleLike {
	readonly fd: number;
}

export const fdTable = new Map<number, HandleLike>();
