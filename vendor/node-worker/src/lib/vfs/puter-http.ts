// The puter api, from the page.
//
// This used to live in the worker and reach the api with two transports: `fetch` for the
// async fs surface and a *blocking* `XMLHttpRequest` for the synchronous one. Only the
// first survives — the blocking transport was never about puterfs, it was about
// `readFileSync`, and that need is served by the service-worker bridge now. So there is one
// transport here, and it is an ordinary `fetch`.
//
// Worth noting that nothing about how these calls reach the network changed: the worker
// deliberately used the *native* pre-proxy `fetch` snapshot (epoxy replaces
// `globalThis.fetch` with a WISP tunnel for user code), so these requests were already
// going out the browser's own stack.

import { fsError, type WireError } from "../../vfs/errno";
import { toWireError } from "../../vfs/errno";
import type { FsEntry, WireCtx } from "../../vfs/entry";

export const DEFAULT_API_ORIGIN = "https://api.puter.com";

export type PuterBodyInit = Record<string, any> | ((data: FormData) => void);
export type PuterHeaders = Record<string, string>;

export function getRandomId(): string {
	return [...Array(16)].reduce((a) => a + Math.random().toString(36)[2], "");
}

// -------------------------------------------------------------- entry shapes

/**
 * puterfs timestamps are unix *seconds*. Missing or garbage becomes 0 (the epoch) rather
 * than NaN: node's stat never yields an Invalid Date, and a NaN here silently poisons
 * every `mtime` comparison downstream.
 */
function toMs(v: unknown): number {
	const n = Number(v);
	return Number.isFinite(n) ? n * 1000 : 0;
}

/**
 * Accepts either wire shape:
 *   - v2 camelCase, from `/fs/readdir` (`isDir`, `modified`, …)
 *   - v1 snake_case, from the legacy `/stat` and `/readdir` routes (`is_dir`, and
 *     `is_symlink` as an int 0|1)
 *
 * Neither has ever had `created_at`/`updated_at`, despite what this runtime used to read —
 * the fields are `created`/`modified`/`accessed`.
 */
export function normalizeFsEntry(raw: any): FsEntry {
	return {
		path: raw.path,
		name: raw.name,
		uid: raw.uid ?? raw.uuid ?? raw.id,
		isDir: Boolean(raw.isDir ?? raw.is_dir),
		isSymlink: Boolean(raw.isSymlink ?? raw.is_symlink),
		size: Number(raw.size ?? 0),
		modifiedMs: toMs(raw.modified),
		createdMs: toMs(raw.created),
		accessedMs: toMs(raw.accessed),
	};
}

/**
 * The request body every `stat` call sends.
 *
 * `return_size` is deliberately absent. It only does anything for directories, where the
 * backend answers it with `SUM(size)` over the entire subtree — an O(descendants) index
 * scan — so a single `statSync` on a project root makes the server walk all of
 * node_modules. node reports a directory's `Stats.size` as a block count, never a subtree
 * total, so the field was never usable anyway.
 */
export function statRequest(path: string) {
	return {
		path,
		return_permissions: false,
		return_versions: false,
		consistency: "strong",
	};
}

/** Monotonic per-request token for the `_` parameter on cacheable GETs. */
let cacheBuster = 0;
export function cacheBust(): string {
	return String(cacheBuster++);
}

/**
 * The url every file read GETs, cache-busted.
 *
 * The api answers `/read` with `ETag` and `Last-Modified` but *no* `Cache-Control`, which
 * is precisely the case where a browser is allowed to invent its own freshness lifetime
 * (RFC 9111 heuristic caching) and serve the body out of the disk cache without
 * revalidating. The bytes on disk then outlive the file: rewrite it and the next read still
 * returns the old version, which is what breaks HMR.
 *
 * `_` is inert on the server: the legacy `/read` handler dispatches on `file` alone. This
 * stays a CORS-simple GET, so it costs no preflight — unlike a `Cache-Control: no-cache`
 * request header, which would.
 */
export function readUrl(path: string): string {
	return `read?file=${encodeURIComponent(path)}&_=${cacheBust()}`;
}

// ----------------------------------------------------------- error mapping

/**
 * Puter api error codes → node errno names. Codes are defined in the backend at
 * `src/backend/src/api/APIError.js`.
 *
 * Only the node code is recorded; the errno and the bare message come from `ERRNO` in
 * ../../vfs/errno.ts, so those facts exist once.
 */
const PUTER_TO_NODE: Record<string, string> = {
	subject_does_not_exist: "ENOENT",
	source_does_not_exist: "ENOENT",
	dest_does_not_exist: "ENOENT",
	shortcut_target_not_found: "ENOENT",
	offset_without_existing_file: "ENOENT",
	item_with_same_name_exists: "EEXIST",
	forbidden: "EACCES",
	permission_denied: "EACCES",
	immutable: "EACCES",
	not_empty: "ENOTEMPTY",
	dest_is_not_a_directory: "ENOTDIR",
	readdir_of_non_directory: "ENOTDIR",
	cannot_read_a_directory: "EISDIR",
	cannot_overwrite_a_directory: "EISDIR",
	invalid_file_name: "EINVAL",
	unresolved_relative_path: "EINVAL",
	invalid_operation: "EINVAL",
	// The api's catch-all for a malformed request. Reachable from normal code: a recursive
	// readdir of `/` returns it (see ./puter-readdir.ts).
	bad_request: "EINVAL",
	cannot_move_item_into_itself: "EINVAL",
	cannot_copy_item_into_itself: "EINVAL",
	source_and_dest_are_the_same: "EINVAL",
	cannot_move_to_root: "EPERM",
	cannot_copy_to_root: "EPERM",
	cannot_write_to_root: "EPERM",
	storage_limit_reached: "ENOSPC",
	file_too_large: "EFBIG",
	not_yet_supported: "ENOTSUP",
	missing_filesystem_capability: "ENOTSUP",
};

/**
 * Turn a non-`ok` response body into the node error for it. Every failure path in the
 * provider goes through here, so the mapping lives in one place.
 *
 * An unrecognized code becomes EIO rather than an uncoded `Error`: node's `fs` never throws
 * without a code, and this tree is full of `catch (e) { if (e.code !== "ENOENT") throw e }`.
 */
export function failPuter(body: any, ctx: WireCtx): never {
	const code = PUTER_TO_NODE[body?.code];
	if (code) throw fsError(code, ctx);
	throw fsError("EIO", {
		...ctx,
		message: body?.message ?? `${ctx.syscall} failed on ${ctx.reportPath}`,
	});
}

export function puterErrorEnvelope(body: any, ctx: WireCtx): WireError {
	try {
		failPuter(body, ctx);
	} catch (err) {
		return toWireError(err, ctx.syscall);
	}
}

// --------------------------------------------------------------- rate limiting

/**
 * The one status this retries, and the reason it is the only one.
 *
 * A 429 says the request was *refused without being performed*, which is what makes replaying
 * it safe — including for a `POST /write` or `/move`, since nothing in this api is idempotent
 * and nothing here can make it so. A 502 or 504 says nothing about whether the mutation
 * landed, so retrying one could duplicate it; those are reported as they arrive.
 */
const THROTTLED = 429;
/** One original attempt plus this many retries. */
const RETRY_ATTEMPTS = 3;
const RETRY_BASE_MS = 300;
const RETRY_MAX_MS = 4000;
const RETRY_JITTER = 0.5;
/**
 * Shortest pause a 429 can buy, however little the server asked for.
 *
 * `Retry-After` as an http-date has *second* granularity, so "half a second from now" is sent
 * as a timestamp that has already passed and parses as a zero wait — which would turn the
 * retries into a tight loop against a server that just said it was overloaded. Only ever binds
 * on a `Retry-After`; the exponential path starts an order of magnitude above it.
 */
const RETRY_MIN_MS = 50;
/**
 * Total time one call will spend waiting before it gives up and reports the 429.
 *
 * Most of these have a caller parked in a *synchronous* `readdirSync` behind them, so the wait
 * is a frozen worker rather than an idle promise. A `Retry-After` longer than what is left of
 * this budget is honoured by giving up rather than by sleeping through it.
 */
const RETRY_BUDGET_MS = 8000;

/** Counter keys that are not endpoints. Parenthesized so they cannot collide with a path. */
const THROTTLE_KEY = "(429 retried)";
const THROTTLE_GAVE_UP_KEY = "(429 gave up)";

/**
 * `Retry-After`, in milliseconds, when the server sent one *and* we are allowed to read it.
 *
 * Usually neither. It is not a CORS-safelisted response header, so on a cross-origin reply it
 * is invisible here unless the backend names it in `Access-Control-Expose-Headers` — which is
 * why the exponential fallback below is the path that actually runs, not the edge case.
 */
function retryAfterMs(res: Response): number | undefined {
	const raw = res.headers.get("Retry-After");
	if (!raw) return undefined;
	const seconds = Number(raw);
	if (Number.isFinite(seconds)) return Math.max(0, seconds * 1000);
	const when = Date.parse(raw);
	return Number.isFinite(when) ? Math.max(0, when - Date.now()) : undefined;
}

function backoffMs(attempt: number): number {
	const base = Math.min(RETRY_BASE_MS * 2 ** attempt, RETRY_MAX_MS);
	// Jittered because these arrive in bursts: a directory walk has dozens of requests in
	// flight, and an unjittered backoff would have all of them return at the same instant and
	// reproduce the burst that earned the 429.
	return base * (1 + RETRY_JITTER * (Math.random() * 2 - 1));
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
	return new Promise((resolve, reject) => {
		if (signal?.aborted) {
			reject(signal.reason);
			return;
		}
		const onAbort = () => {
			clearTimeout(timer);
			reject(signal!.reason);
		};
		const timer = setTimeout(() => {
			signal?.removeEventListener("abort", onAbort);
			resolve();
		}, ms);
		signal?.addEventListener("abort", onAbort, { once: true });
	});
}

// ------------------------------------------------------------- the transport

export interface PuterResponse {
	ok: boolean;
	status: number;
	bytes: Uint8Array;
	/** The body parsed as JSON, or undefined when it is not JSON. Decoded once. */
	json(): any;
}

const decoder = new TextDecoder("utf-8");

function makeResponse(
	ok: boolean,
	status: number,
	bytes: Uint8Array
): PuterResponse {
	let decoded: any;
	let tried = false;
	return {
		ok,
		status,
		bytes,
		json() {
			if (!tried) {
				tried = true;
				try {
					decoded = JSON.parse(decoder.decode(bytes));
				} catch {
					decoded = undefined;
				}
			}
			return decoded;
		},
	};
}

function handleBody(bodyInit?: PuterBodyInit): string | FormData | undefined {
	if (!bodyInit) return undefined;
	if (bodyInit instanceof Function) {
		const form = new FormData();
		bodyInit(form);
		return form;
	}
	return JSON.stringify(bodyInit);
}

export class PuterApi {
	#token: string;
	#origin: string;
	/**
	 * Per-endpoint call counts, keyed by the path with the query string stripped. The
	 * whole point of the resolver and readdir work is to make this number go down, and
	 * there is no other way to see it — it rides back to the worker on the reply frame so
	 * `NODE_WORKER_API_STATS` keeps reporting it after the move.
	 */
	#counts = new Map<string, number>();
	/**
	 * When the last 429 said it was worth asking again. Shared across every call, since the
	 * limit is on the account rather than on any one request.
	 */
	#cooldownUntil = 0;
	/** Snapshot of `#counts` at the last `drainStats`, so the next one reports a delta. */
	#drained: Map<string, number> | undefined;

	constructor(token: string, origin: string = DEFAULT_API_ORIGIN) {
		this.#token = token;
		this.#origin = origin;
	}

	get origin() {
		return this.#origin;
	}

	stats(): Record<string, number> {
		return Object.fromEntries([...this.#counts].sort((a, b) => b[1] - a[1]));
	}

	resetStats() {
		this.#counts.clear();
	}

	/**
	 * Counts since the last drain, and reset.
	 *
	 * The per-reply sideband needs a *delta*: it reports what one call cost, so handing it
	 * the cumulative total would make every reply restate the whole run. `stats()` stays
	 * cumulative for `apiStats()`, which is a different question — what has this page sent
	 * altogether.
	 */
	drainStats(): Record<string, number> | undefined {
		if (this.#drained === undefined) this.#drained = new Map();
		const out: Record<string, number> = {};
		let any = false;
		for (const [key, total] of this.#counts) {
			const delta = total - (this.#drained.get(key) ?? 0);
			if (delta <= 0) continue;
			out[key] = delta;
			any = true;
		}
		if (!any) return undefined;
		this.#drained = new Map(this.#counts);
		return out;
	}

	/**
	 * A GET with no custom request headers is a CORS-*simple* request, so the browser skips
	 * the preflight entirely. Carrying the token as `?auth_token=` instead of an
	 * `Authorization` header is what keeps it simple (the api accepts either), and it halves
	 * the round trips. POSTs always send `Content-Type: application/json`, which preflights
	 * regardless, so they keep the header.
	 */
	#url(path: string, method: string, headers: Record<string, string>): string {
		const url = new URL(`${this.#origin}/${path}`);
		if (method === "GET") url.searchParams.append("auth_token", this.#token);
		else headers["Authorization"] = "Bearer " + this.#token;
		return url.toString();
	}

	async fetch(
		path: string,
		bodyInit?: PuterBodyInit,
		signal?: AbortSignal,
		extraHeaders?: PuterHeaders
	): Promise<PuterResponse> {
		const method = bodyInit ? "POST" : "GET";
		const headers: Record<string, string> =
			bodyInit && !(bodyInit instanceof Function)
				? { "Content-Type": "application/json" }
				: {};
		if (extraHeaders) Object.assign(headers, extraHeaders);

		const res = await this.#send(
			path,
			this.#url(path, method, headers),
			{ headers, method, body: handleBody(bodyInit), signal },
			signal
		);
		return makeResponse(
			res.ok,
			res.status,
			new Uint8Array(await res.arrayBuffer())
		);
	}

	/**
	 * Like `fetch`, but hands back the un-consumed `Response` so the caller can read the
	 * body incrementally. `openRead` uses this to serve a whole file from one request
	 * instead of a ranged GET per chunk.
	 */
	async fetchStream(
		path: string,
		signal?: AbortSignal,
		extraHeaders?: PuterHeaders
	): Promise<Response> {
		const headers: Record<string, string> = {};
		if (extraHeaders) Object.assign(headers, extraHeaders);
		return this.#send(
			path,
			this.#url(path, "GET", headers),
			{ headers, method: "GET", signal },
			signal
		);
	}

	/**
	 * One request, with 429 retried. Everything in this class goes through here, the streaming
	 * read included, because a rate limit is about the account and not about the endpoint.
	 */
	async #send(
		path: string,
		url: string,
		init: RequestInit,
		signal?: AbortSignal
	): Promise<Response> {
		const deadline = Date.now() + RETRY_BUDGET_MS;
		for (let attempt = 0; ; attempt++) {
			await this.#awaitCooldown(deadline, signal);
			this.#count(path);
			const res = await fetch(url, init);
			if (res.status !== THROTTLED) return res;

			const wait = Math.max(
				RETRY_MIN_MS,
				retryAfterMs(res) ?? backoffMs(attempt)
			);
			if (attempt >= RETRY_ATTEMPTS || Date.now() + wait > deadline) {
				this.#bump(THROTTLE_GAVE_UP_KEY);
				return res;
			}
			this.#bump(THROTTLE_KEY);
			// The limit is the account's, not this request's, so hold the others back too.
			// Without it every call already in flight spends its own attempts rediscovering
			// the same limit, and the retries are the burst all over again.
			this.#cooldownUntil = Math.max(this.#cooldownUntil, Date.now() + wait);
			// The body is a rejection notice nobody reads, and leaving it unconsumed keeps
			// the connection checked out.
			res.body?.cancel().catch(() => {});
			await sleep(wait, signal);
		}
	}

	/**
	 * Wait out a cooldown another request's 429 established.
	 *
	 * Capped by our own deadline: past it there is nothing left to gain by waiting, so the
	 * request goes out and its 429, if it comes, is reported to the caller.
	 */
	async #awaitCooldown(deadline: number, signal?: AbortSignal) {
		const wait = Math.min(
			this.#cooldownUntil - Date.now(),
			deadline - Date.now()
		);
		if (wait > 0) await sleep(wait, signal);
	}

	#count(path: string) {
		this.#bump(path.split("?")[0]);
	}

	#bump(key: string) {
		this.#counts.set(key, (this.#counts.get(key) ?? 0) + 1);
	}
}
