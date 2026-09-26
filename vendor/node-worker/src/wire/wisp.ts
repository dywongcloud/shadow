// Where `node:net` / `node:tls` get their TCP: ONE Wisp relay URL.
//
// Everything above the socket — `net.connect`, `tls.connect`, `http.request`,
// `fetch` in a worker with no puter token — bottoms out in epoxy, which tunnels
// TCP (and terminates TLS itself, so TLS needs no configuration of its own)
// over a Wisp websocket. So the whole of "does this worker have a network" is
// the answer to "which relay URL do we dial", and that is what this module
// decides.
//
// ORDER OF SOURCES, and why. The URL is the one thing on this path that can put
// a THIRD PARTY in front of a user's traffic, so it is resolved deliberately
// rather than defaulted:
//
//   1. `net.wispUrl` — what the caller of `NodeWorker.create` passed, dialed as
//      given (a wisp v1 URL carries its relay token in the path, so nothing is
//      parsed out of it).
//   2. `globalThis.HIVE_WISP_URL` — the platform-configured relay. The host page
//      writes it here from the operator's `HIVE_BROWSER_WISP_URL`, which
//      reaches the page in the admission capability's `net` block; a host that
//      never sets it simply does not participate.
//   3. the PUBLIC fallback relay, only when
//      `globalThis.HIVE_WISP_PUBLIC_FALLBACK === true` — the upstream README's
//      example relay, i.e. infrastructure neither this package nor the host
//      controls. Because it is a third party it is (a) off unless asked for,
//      (b) PROVEN reachable by a bounded websocket handshake before any donor
//      traffic is routed at it, and (c) named in a console warning every time
//      it is chosen — disclosed, never silently enabled.
//   4. none of the above — no URL. That is not an error at startup (a worker
//      that never opens a socket has no network problem), but the first socket
//      throws `WispRelayUnavailable` NAMING what is missing: a relay-less
//      worker must never look like one with a broken network.
//
// The one case that DOES throw here is the fallback being enabled and not
// answering: the caller asked for a network and there is now proof that the
// only source left cannot carry one.

import type { NodeNetInit } from "./control";

/** The public fallback: upstream node-worker's own README example. */
export const PUBLIC_WISP_FALLBACK_URL = "wss://anura.pro/";

/** How long the fallback relay gets to answer a handshake. `0` disables the probe. */
export const DEFAULT_PUBLIC_FALLBACK_PROBE_MS = 5_000;

/**
 * No relay to dial, named — as opposed to an opaque socket failure.
 *
 * The `name` is the contract: a host matching `err.name === "WispRelayUnavailable"`
 * can tell "this platform has no relay configured" apart from "the relay is down"
 * and from "the program dialed a bad host", which is the difference between an
 * operator's to-do and a guest program's bug.
 */
export class WispRelayUnavailable extends Error {
	constructor(message: string) {
		super(message);
		this.name = "WispRelayUnavailable";
	}
}

export type WispSource = "caller" | "platform" | "public-fallback";

export interface ResolvedWisp {
	/** The relay address, dialed as given. */
	url: string;
	/** Which source won — carried so a caller can disclose it honestly. */
	source: WispSource;
	/** True only for a relay this platform does not control. */
	thirdParty: boolean;
}

/**
 * A `ws:` or `wss:` URL with a host, and nothing that could smuggle a second
 * origin in — the string is dialed verbatim.
 */
export function isWispUrl(value: unknown): value is string {
	if (typeof value !== "string") return false;
	const raw = value.trim();
	if (!raw || /\s/.test(raw)) return false;
	const rest = raw.startsWith("wss://")
		? raw.slice("wss://".length)
		: raw.startsWith("ws://")
			? raw.slice("ws://".length)
			: undefined;
	if (rest === undefined) return false;
	// Authority is everything before the first path, query or fragment
	// delimiter: past it, the string is the relay's own (a wisp v1 URL carries
	// its token in the path), before it, it has to be a host with an optional
	// numeric port.
	const authority = rest.split(/[/?#]/)[0].split("@").pop() ?? "";
	if (!authority) return false;
	const port = authority.split(":").pop();
	if (authority.includes(":") && !/^\d+$/.test(port ?? "")) return false;
	return true;
}

/** The platform-configured relay, published by the host page. See source 2. */
export function platformWispUrl(): string | undefined {
	const value = (globalThis as { HIVE_WISP_URL?: unknown }).HIVE_WISP_URL;
	return isWispUrl(value) ? value.trim() : undefined;
}

/** The fallback relay's address: `net`'s, then the host's, then the default. */
export function publicFallbackUrl(net?: NodeNetInit): string {
	const override = (net as { publicFallbackUrl?: unknown } | undefined)?.publicFallbackUrl;
	if (isWispUrl(override)) return override.trim();
	const value = (globalThis as { HIVE_WISP_PUBLIC_FALLBACK_URL?: unknown })
		.HIVE_WISP_PUBLIC_FALLBACK_URL;
	return isWispUrl(value) ? value.trim() : PUBLIC_WISP_FALLBACK_URL;
}

/**
 * Whether the third-party fallback may be used at all.
 *
 * Opt-in on either side: per-worker (`net.allowPublicFallback`) or fleet-wide
 * (`globalThis.HIVE_WISP_PUBLIC_FALLBACK`, written from the host's own config).
 * Absent means no — the default has to be the safe one, because the cost of
 * guessing wrong here is a third party reading someone's traffic.
 */
export function publicFallbackAllowed(net?: NodeNetInit): boolean {
	return (
		net?.allowPublicFallback === true ||
		(globalThis as { HIVE_WISP_PUBLIC_FALLBACK?: unknown })
			.HIVE_WISP_PUBLIC_FALLBACK === true
	);
}

/**
 * Prove a relay answers, by completing a websocket handshake and hanging up.
 *
 * A handshake is the only liveness proof available from a page: wisp has no
 * health endpoint every relay is obliged to serve, and a relay that upgrades is
 * a relay that can carry a stream. Bounded, because this runs before a worker
 * starts and a silent third party must not be able to stall it.
 */
export async function probeWispRelay(url: string, timeoutMs: number): Promise<void> {
	if (timeoutMs <= 0) return;
	let socket: WebSocket;
	try {
		socket = new WebSocket(url);
	} catch (err) {
		throw new Error(`could not open a websocket to ${url} (${(err as Error)?.message ?? err})`);
	}
	try {
		await new Promise<void>((resolve, reject) => {
			const timer = setTimeout(
				() => reject(new Error(`no handshake in ${timeoutMs}ms`)),
				timeoutMs,
			);
			const settle = (err?: Error) => {
				clearTimeout(timer);
				if (err) reject(err);
				else resolve();
			};
			socket.onopen = () => settle();
			socket.onerror = () => settle(new Error("websocket error"));
			socket.onclose = (event: CloseEvent) =>
				settle(new Error(`closed before opening (code ${event.code})`));
		});
	} finally {
		try {
			socket.close();
		} catch {
			/* already gone — the probe is over either way */
		}
	}
}

/**
 * Which relay this anonymous worker dials, or `undefined` for none.
 *
 * Never called when a puter token was passed: a token mints relay credentials
 * of its own, and `net` is documented as ignored in that case.
 */
export async function resolveWispRelay(net?: NodeNetInit): Promise<ResolvedWisp | undefined> {
	const explicit = net?.wispUrl?.trim();
	if (explicit) {
		if (!isWispUrl(explicit)) {
			throw new WispRelayUnavailable(
				`net.wispUrl is not a ws:// or wss:// URL: ${JSON.stringify(net?.wispUrl)}`,
			);
		}
		return { url: explicit, source: "caller", thirdParty: false };
	}

	const platform = platformWispUrl();
	if (platform) return { url: platform, source: "platform", thirdParty: false };

	if (!publicFallbackAllowed(net)) return undefined;

	const url = publicFallbackUrl(net);
	const timeoutMs = net?.publicFallbackProbeMs ?? DEFAULT_PUBLIC_FALLBACK_PROBE_MS;
	try {
		await probeWispRelay(url, timeoutMs);
	} catch (err) {
		throw new WispRelayUnavailable(
			`no wisp relay is reachable: the public fallback ${url} did not complete a websocket ` +
				`handshake in ${timeoutMs}ms (${(err as Error)?.message ?? err}), and no platform relay ` +
				`is configured — node:net / node:tls are unavailable in this worker`,
		);
	}
	// DISCLOSURE, operator-facing and unconditional: a third party is about to
	// terminate and re-emit every connection this worker makes. Saying so is the
	// whole difference between a fallback and a silent MITM.
	console.warn(
		`[node-worker] node:net / node:tls are routed through the THIRD-PARTY public wisp relay ` +
			`${url}: every outbound connection from this worker is terminated and re-emitted there. ` +
			`Configure a platform relay to keep this traffic on infrastructure you control.`,
	);
	return { url, source: "public-fallback", thirdParty: true };
}

/** The error a socket throws when no relay was ever resolved. */
export function noRelayError(): WispRelayUnavailable {
	return new WispRelayUnavailable(
		`no wisp relay is configured, so node:net / node:tls cannot open a socket: pass ` +
			`net: { wispUrl } to NodeWorker.create, or have the platform publish one ` +
			`(HIVE_BROWSER_WISP_URL on the fleet, written here as globalThis.HIVE_WISP_URL)`,
	);
}
