// Directory listing against the api's `/fs/readdir` route, which can return a whole
// subtree in one paged call (`recursive` + `depth`).
//
// The paging, depth-horizon and error handling live here rather than being written out at
// each call site — this was the first operation to get that treatment, back when it had to
// be a generator so the sync and async fs surfaces could share it. Being ordinary async
// code now costs it nothing: the *reason* for one copy was never the transport, it was that
// this logic is fiddly enough to drift if duplicated.

import { MAX_DEPTH, relDepth } from "../../vfs/path";
import type { FsEntry, WireCtx } from "../../vfs/entry";
import {
	cacheBust,
	failPuter,
	normalizeFsEntry,
	type PuterApi,
} from "./puter-http";

/**
 * The api caps `limit` at 10k and defaults to 1k. Responses are parsed with a single
 * non-streaming `JSON.parse`, so oversized pages cost a big transient string; 5k is a
 * compromise between that and the per-request round trip.
 */
const PAGE_LIMIT = 5000;

export interface ReaddirPage {
	/** Every descendant returned for this root, across all pages. */
	entries: FsEntry[];
	/**
	 * Whether paging ran to completion. A walk cut short by `maxEntries` returns a valid
	 * prefix of the listing but is *not* an exhaustive one — callers that derive "this
	 * directory contains nothing else" from a listing must check this first.
	 */
	complete: boolean;
}

interface PagesOptions {
	recursive?: boolean;
	depth?: number;
	/** Stop paging once this many entries have accumulated. */
	maxEntries?: number;
}

function readdirUrl(
	path: string,
	opts: {
		recursive?: boolean;
		depth?: number;
		cursor?: string;
		includeTotal?: boolean;
	}
): string {
	const q = new URLSearchParams();
	q.set("path", path);
	if (opts.recursive) {
		q.set("recursive", "true");
		q.set("depth", String(opts.depth ?? MAX_DEPTH));
	}
	if (opts.includeTotal) q.set("includeTotal", "true");
	q.set("limit", String(PAGE_LIMIT));
	// Always present, empty on the first page. Sending `cursor` at all is what opts into
	// the `{items, cursor?}` envelope; without it a non-recursive listing comes back as a
	// bare array that the server has already truncated to `limit` with no way to ask for
	// the rest.
	q.set("cursor", opts.cursor ?? "");
	// Cache-busted for the same reason `readUrl` is: a silently stale listing is a
	// miserable bug to chase and it costs one query parameter.
	q.set("_", cacheBust());
	// GET rather than POST: it carries the token as `?auth_token=`, which keeps it a
	// CORS-simple request and skips the preflight.
	return `fs/readdir?${q}`;
}

/**
 * A recursive response is always the `{items, cursor?, total?}` envelope; the non-recursive
 * form is a bare array. Both reduce to this.
 */
function toPage(body: any): {
	items: any[];
	cursor: string | undefined;
	total: number | undefined;
} {
	if (Array.isArray(body)) {
		return { items: body, cursor: undefined, total: undefined };
	}
	return {
		items: body?.items ?? [],
		cursor: body?.cursor ?? undefined,
		total: typeof body?.total === "number" ? body.total : undefined,
	};
}

/**
 * List one directory, following the cursor to the end. With `recursive` this is the whole
 * subtree down to `depth` (default and maximum {@link MAX_DEPTH}), excluding the root
 * itself.
 */
export async function readdirPages(
	api: PuterApi,
	ctx: WireCtx,
	root: string,
	opts: PagesOptions = {}
): Promise<ReaddirPage> {
	const maxEntries = opts.maxEntries ?? Infinity;
	const entries: FsEntry[] = [];
	let cursor: string | undefined;
	let first = true;

	do {
		const res = await api.fetch(
			readdirUrl(root, {
				recursive: opts.recursive,
				depth: opts.depth,
				cursor,
				// Ask the server to count the subtree on the first page whenever a budget is
				// in play, so an oversized listing is abandoned after one page instead of
				// after `maxEntries`-worth of them. Costs one COUNT(*) and no extra round
				// trip.
				includeTotal: first && maxEntries !== Infinity,
			})
		);
		const body = res.json();
		if (!res.ok) failPuter(body, ctx);

		const page = toPage(body);
		if (first && page.total !== undefined && page.total > maxEntries) {
			return { entries, complete: false };
		}
		first = false;

		for (const item of page.items) entries.push(normalizeFsEntry(item));
		cursor = page.cursor;

		if (entries.length >= maxEntries) return { entries, complete: false };
		// Only a null/absent cursor means "last page" — `items.length < limit` does not,
		// and treating it as such truncates listings at random.
	} while (cursor !== undefined && cursor !== null);

	return { entries, complete: true };
}

/**
 * Every descendant of `root`, at any depth.
 *
 * The api caps a single recursive call at {@link MAX_DEPTH} levels, so any directory
 * returned at exactly that depth becomes a new root and is walked again. No de-duplication
 * is needed: a call rooted at `r` excludes `r` itself, so a follow-up rooted at a horizon
 * directory returns a disjoint set.
 *
 * Entries come back ordered by full path, ascending — the api's ordering. node guarantees
 * no particular order.
 */
export async function readdirTree(
	api: PuterApi,
	ctx: WireCtx,
	root: string
): Promise<FsEntry[]> {
	const out: FsEntry[] = [];
	let frontier: string[];

	if (root === "/") {
		// The api refuses a recursive listing at the root (400 bad_request — it would be a
		// prefix scan over every user). Enumerate it flat instead and treat each top-level
		// directory as its own recursive root.
		const page = await readdirPages(api, ctx, "/", { recursive: false });
		out.push(...page.entries);
		frontier = page.entries.filter((e) => e.isDir).map((e) => e.path);
	} else {
		frontier = [root];
	}

	while (frontier.length > 0) {
		const current = frontier.shift()!;
		const page = await readdirPages(api, ctx, current, { recursive: true });
		out.push(...page.entries);

		for (const entry of page.entries) {
			if (entry.isDir && relDepth(current, entry.path) === MAX_DEPTH) {
				frontier.push(entry.path);
			}
		}
	}

	return out;
}
