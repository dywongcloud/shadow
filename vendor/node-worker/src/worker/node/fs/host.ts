// The filesystem, as the `fs` surface calls it.
//
// Two namespaces over one wire protocol: `host.*` blocks (a synchronous XHR the service
// worker relays to the page) and `hostAsync.*` awaits (a postMessage round trip). Both send
// the same frame to the same dispatcher, so there is one implementation of every filesystem
// operation and it lives on the host — next to the mount table and the providers.
//
// ## What this replaces
//
// There used to be a generator protocol here: an operation *described* the requests it needed
// and a driver performed them with either transport, so one implementation could serve
// `readFileSync` and `fs.promises.readFile`. That existed because the filesystem lived in the
// worker and had to satisfy a blocking caller, which meant it could not `await` — which in
// turn meant no backend could either, and OPFS, the File System Access api and IndexedDB were
// all structurally excluded.
//
// With the filesystem host-side there is nothing left for a generator to describe: every
// operation is exactly one round trip, including the composed ones (`append`, `truncate`,
// `cp`, `mkdtemp`, `exists`) that used to cost one per step. So the protocol collapses to
// these two tables, and what `Plan` was preventing — two copies of the filesystem *logic* —
// cannot happen anyway, because that logic is not here.
//
// The duplication that remains is deliberate and was always there: ../sync.ts and
// ../promises.ts each do their own argument coercion, which is node's overload sets rather
// than filesystem behaviour.

import nodeBuffer from "../buffer";
import type {
	FsEntry,
	Listing,
	ReaddirOpts,
	WireCtx,
} from "../../../vfs/entry";
import { answerBytes, mountFor, vfsAsync, vfsSync } from "./transport";

let Buffer = nodeBuffer.Buffer;

/**
 * Builds the per-operation context the host needs for error reporting. `syscall` is node's
 * name for the operation and shows up in `err.syscall` and the message.
 */
export function ctx(syscall: string, reportPath: string): WireCtx {
	return { syscall, reportPath };
}

/** Bytes as a `Buffer`, which is what the `fs` surface hands back. */
function buf(bytes: Uint8Array): Buffer {
	return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

// ------------------------------------------------------------------- blocking

export const host = {
	stat(c: WireCtx, path: string): FsEntry {
		return vfsSync({ op: "stat", ctx: c, path }).value;
	},
	access(c: WireCtx, path: string): void {
		vfsSync({ op: "access", ctx: c, path });
	},
	exists(c: WireCtx, path: string): boolean {
		return vfsSync({ op: "exists", ctx: c, path }).value;
	},
	statfs(c: WireCtx, path: string): { used: number; capacity: number } {
		return vfsSync({ op: "statfs", ctx: c, path }).value;
	},
	readdir(c: WireCtx, path: string, opts?: ReaddirOpts): Listing {
		return vfsSync({ op: "readdir", ctx: c, path, opts }).value;
	},
	readFile(c: WireCtx, path: string): Buffer {
		return buf(answerBytes(vfsSync({ op: "readFile", ctx: c, path })));
	},
	readRange(c: WireCtx, path: string, offset: number, length: number): Buffer {
		return buf(
			answerBytes(vfsSync({ op: "readRange", ctx: c, path, offset, length }))
		);
	},
	writeFile(c: WireCtx, path: string, data: Uint8Array): void {
		vfsSync({ op: "writeFile", ctx: c, path }, [data]);
	},
	append(c: WireCtx, path: string, data: Uint8Array): void {
		vfsSync({ op: "append", ctx: c, path }, [data]);
	},
	mkdir(c: WireCtx, path: string, recursive: boolean): string | undefined {
		return vfsSync({ op: "mkdir", ctx: c, path, recursive }).value;
	},
	rm(c: WireCtx, path: string, recursive: boolean, force: boolean): void {
		vfsSync({ op: "rm", ctx: c, path, recursive, force });
	},
	rename(c: WireCtx, from: string, to: string): void {
		vfsSync({ op: "rename", ctx: c, from, to });
	},
	copyFile(c: WireCtx, from: string, to: string, overwrite: boolean): void {
		vfsSync({ op: "copyFile", ctx: c, from, to, overwrite });
	},
	/** Already validates a missing path, so callers need no follow-up stat. */
	utimes(c: WireCtx, path: string, atimeMs: number, mtimeMs: number): boolean {
		return vfsSync({ op: "utimes", ctx: c, path, atimeMs, mtimeMs }).value;
	},
	truncate(c: WireCtx, path: string, length: number): void {
		vfsSync({ op: "truncate", ctx: c, path, length });
	},
	mkdtemp(c: WireCtx, prefix: string): string {
		return vfsSync({ op: "mkdtemp", ctx: c, prefix }).value;
	},
	cp(
		c: WireCtx,
		from: string,
		to: string,
		o: { recursive: boolean; force: boolean; errorOnExist: boolean }
	): void {
		vfsSync({ op: "cp", ctx: c, from, to, ...o });
	},

	// --- the fd family ---

	open(c: WireCtx, path: string, flags: string | number): number {
		return vfsSync({ op: "open", ctx: c, path, flags }).value.fd;
	},
	close(c: WireCtx, fd: number): void {
		vfsSync({ op: "close", ctx: c, fd });
	},
	read(
		c: WireCtx,
		fd: number,
		length: number,
		position: number | null
	): Buffer {
		return buf(
			answerBytes(vfsSync({ op: "fdRead", ctx: c, fd, length, position }))
		);
	},
	readv(
		c: WireCtx,
		fd: number,
		lengths: number[],
		position: number | null
	): Uint8Array[] {
		return vfsSync({ op: "fdReadv", ctx: c, fd, lengths, position }).parts;
	},
	write(
		c: WireCtx,
		fd: number,
		data: Uint8Array,
		position: number | null
	): number {
		return vfsSync({ op: "fdWrite", ctx: c, fd, position }, [data]).value;
	},
	writev(
		c: WireCtx,
		fd: number,
		chunks: Uint8Array[],
		position: number | null
	): number {
		return vfsSync({ op: "fdWritev", ctx: c, fd, position }, chunks).value;
	},
	readFileFd(c: WireCtx, fd: number): Buffer {
		return buf(answerBytes(vfsSync({ op: "fdReadFile", ctx: c, fd })));
	},
	writeFileFd(c: WireCtx, fd: number, data: Uint8Array): void {
		vfsSync({ op: "fdWriteFile", ctx: c, fd }, [data]);
	},
	appendFd(c: WireCtx, fd: number, data: Uint8Array): void {
		vfsSync({ op: "fdAppend", ctx: c, fd }, [data]);
	},
	fstat(c: WireCtx, fd: number): FsEntry {
		return vfsSync({ op: "fdStat", ctx: c, fd }).value;
	},
	ftruncate(c: WireCtx, fd: number, length: number): void {
		vfsSync({ op: "fdTruncate", ctx: c, fd, length });
	},
	fsync(c: WireCtx, fd: number): void {
		vfsSync({ op: "fdSync", ctx: c, fd });
	},
	futimes(c: WireCtx, fd: number, atimeMs: number, mtimeMs: number): boolean {
		return vfsSync({ op: "fdUtimes", ctx: c, fd, atimeMs, mtimeMs }).value;
	},
	/** write + sync + close, as one round trip. */
	flushWrite(c: WireCtx, fd: number, data?: Uint8Array): void {
		vfsSync({ op: "fdFlushWrite", ctx: c, fd }, data ? [data] : undefined);
	},

	/**
	 * Whether a path's backend has a *real* positioned read, answered from the pushed mount
	 * snapshot rather than a round trip.
	 *
	 * The only capability question left in the worker, and it exists because a caller has to
	 * choose a read strategy before it starts reading.
	 */
	hasNativeRange(path: string): boolean {
		return mountFor(path).hasNativeRange;
	},
};

// ---------------------------------------------------------------------- async

export const hostAsync = {
	async stat(c: WireCtx, path: string, signal?: AbortSignal): Promise<FsEntry> {
		return (await vfsAsync({ op: "stat", ctx: c, path }, undefined, signal))
			.value;
	},
	async access(c: WireCtx, path: string): Promise<void> {
		await vfsAsync({ op: "access", ctx: c, path });
	},
	async exists(c: WireCtx, path: string): Promise<boolean> {
		return (await vfsAsync({ op: "exists", ctx: c, path })).value;
	},
	async statfs(
		c: WireCtx,
		path: string
	): Promise<{ used: number; capacity: number }> {
		return (await vfsAsync({ op: "statfs", ctx: c, path })).value;
	},
	async readdir(
		c: WireCtx,
		path: string,
		opts?: ReaddirOpts
	): Promise<Listing> {
		return (await vfsAsync({ op: "readdir", ctx: c, path, opts })).value;
	},
	async readFile(
		c: WireCtx,
		path: string,
		signal?: AbortSignal
	): Promise<Buffer> {
		return buf(
			answerBytes(
				await vfsAsync({ op: "readFile", ctx: c, path }, undefined, signal)
			)
		);
	},
	async readRange(
		c: WireCtx,
		path: string,
		offset: number,
		length: number
	): Promise<Buffer> {
		return buf(
			answerBytes(
				await vfsAsync({ op: "readRange", ctx: c, path, offset, length })
			)
		);
	},
	async writeFile(
		c: WireCtx,
		path: string,
		data: Uint8Array,
		signal?: AbortSignal
	): Promise<void> {
		await vfsAsync({ op: "writeFile", ctx: c, path }, [data], signal);
	},
	async append(c: WireCtx, path: string, data: Uint8Array): Promise<void> {
		await vfsAsync({ op: "append", ctx: c, path }, [data]);
	},
	async mkdir(
		c: WireCtx,
		path: string,
		recursive: boolean
	): Promise<string | undefined> {
		return (await vfsAsync({ op: "mkdir", ctx: c, path, recursive })).value;
	},
	async rm(
		c: WireCtx,
		path: string,
		recursive: boolean,
		force: boolean
	): Promise<void> {
		await vfsAsync({ op: "rm", ctx: c, path, recursive, force });
	},
	async rename(c: WireCtx, from: string, to: string): Promise<void> {
		await vfsAsync({ op: "rename", ctx: c, from, to });
	},
	async copyFile(
		c: WireCtx,
		from: string,
		to: string,
		overwrite: boolean
	): Promise<void> {
		await vfsAsync({ op: "copyFile", ctx: c, from, to, overwrite });
	},
	async utimes(
		c: WireCtx,
		path: string,
		atimeMs: number,
		mtimeMs: number
	): Promise<boolean> {
		return (await vfsAsync({ op: "utimes", ctx: c, path, atimeMs, mtimeMs }))
			.value;
	},
	async truncate(c: WireCtx, path: string, length: number): Promise<void> {
		await vfsAsync({ op: "truncate", ctx: c, path, length });
	},
	async mkdtemp(c: WireCtx, prefix: string): Promise<string> {
		return (await vfsAsync({ op: "mkdtemp", ctx: c, prefix })).value;
	},
	async cp(
		c: WireCtx,
		from: string,
		to: string,
		o: { recursive: boolean; force: boolean; errorOnExist: boolean }
	): Promise<void> {
		await vfsAsync({ op: "cp", ctx: c, from, to, ...o });
	},

	// --- the fd family ---

	async open(
		c: WireCtx,
		path: string,
		flags: string | number
	): Promise<number> {
		return (await vfsAsync({ op: "open", ctx: c, path, flags })).value.fd;
	},
	async close(c: WireCtx, fd: number): Promise<void> {
		await vfsAsync({ op: "close", ctx: c, fd });
	},
	async read(
		c: WireCtx,
		fd: number,
		length: number,
		position: number | null
	): Promise<Buffer> {
		return buf(
			answerBytes(
				await vfsAsync({ op: "fdRead", ctx: c, fd, length, position })
			)
		);
	},
	async readv(
		c: WireCtx,
		fd: number,
		lengths: number[],
		position: number | null
	): Promise<Uint8Array[]> {
		return (await vfsAsync({ op: "fdReadv", ctx: c, fd, lengths, position }))
			.parts;
	},
	async write(
		c: WireCtx,
		fd: number,
		data: Uint8Array,
		position: number | null
	): Promise<number> {
		return (await vfsAsync({ op: "fdWrite", ctx: c, fd, position }, [data]))
			.value;
	},
	async writev(
		c: WireCtx,
		fd: number,
		chunks: Uint8Array[],
		position: number | null
	): Promise<number> {
		return (await vfsAsync({ op: "fdWritev", ctx: c, fd, position }, chunks))
			.value;
	},
	async readFileFd(c: WireCtx, fd: number): Promise<Buffer> {
		return buf(answerBytes(await vfsAsync({ op: "fdReadFile", ctx: c, fd })));
	},
	async writeFileFd(c: WireCtx, fd: number, data: Uint8Array): Promise<void> {
		await vfsAsync({ op: "fdWriteFile", ctx: c, fd }, [data]);
	},
	async appendFd(c: WireCtx, fd: number, data: Uint8Array): Promise<void> {
		await vfsAsync({ op: "fdAppend", ctx: c, fd }, [data]);
	},
	async fstat(c: WireCtx, fd: number): Promise<FsEntry> {
		return (await vfsAsync({ op: "fdStat", ctx: c, fd })).value;
	},
	async ftruncate(c: WireCtx, fd: number, length: number): Promise<void> {
		await vfsAsync({ op: "fdTruncate", ctx: c, fd, length });
	},
	async fsync(c: WireCtx, fd: number): Promise<void> {
		await vfsAsync({ op: "fdSync", ctx: c, fd });
	},
	async futimes(
		c: WireCtx,
		fd: number,
		atimeMs: number,
		mtimeMs: number
	): Promise<boolean> {
		return (await vfsAsync({ op: "fdUtimes", ctx: c, fd, atimeMs, mtimeMs }))
			.value;
	},
};
