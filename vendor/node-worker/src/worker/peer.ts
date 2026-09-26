import { KIND_PEER } from "../wire/kinds";
import type { PeerResult } from "../wire/peer";
import { call } from "./wire";
import { console_warn } from "./console";
// Straight from ./epoxy/globals rather than the ./epoxy barrel: this module is
// already inside epoxy's import cycle (epoxy/index imports `connectToPeer`), and
// the barrel re-exports `FETCH` from a module it has not evaluated yet at that
// point in its own body.
import { FETCH } from "./epoxy/globals";
import { API_ORIGIN, decode, fetchPuter } from "./puter";
import { PEER_TOKEN, PUTER_TOKEN } from "./state";

interface IceServerState {
	servers: RTCIceServer[];
	fetchedAt: number;
	ttl: number;
}

// `peer/signaller-info` needs no authentication, and hands out a STUN list next to
// the signaller address for exactly the case where the authed `peer/generate-turn`
// is unavailable. So it is fetched natively rather than through `fetchPuter`, which
// would throw "Not authed" on an anonymous run, and its `fallbackIce` is kept.
let signaller: string | undefined;
let signallerIce: RTCIceServer[] | undefined;
async function getSignaller(): Promise<string> {
	if (signaller) return signaller;

	let res = await FETCH(`${API_ORIGIN}/peer/signaller-info`);
	if (!res.ok) throw new Error("failed to get signaller");
	let { url, fallbackIce } = decode(new Uint8Array(await res.arrayBuffer()));
	signaller = url;
	signallerIce = fallbackIce;

	return url;
}

let iceState: IceServerState | undefined;
async function getIceServers(): Promise<RTCIceServer[]> {
	if (iceState && Date.now() - iceState.fetchedAt < iceState.ttl * 1000)
		return iceState.servers;

	// TURN relays are minted per user, so an anonymous peer has no way to ask for
	// them and gets STUN only — which is enough unless both ends are behind a NAT
	// that refuses to be traversed.
	if (!PUTER_TOKEN) {
		await getSignaller();
		if (!signallerIce?.length) {
			throw new Error("failed to get ice servers");
		}
		return signallerIce;
	}

	let [ok, u8array] = await fetchPuter("peer/generate-turn", {});
	if (!ok) throw new Error("failed to get ice servers");
	let { iceServers, ttl, fallbackIce } = decode(u8array);

	if (!iceServers?.length) {
		console_warn("[node-worker] [peer] unable to fetch turn relays");
		iceServers = fallbackIce;
	}

	iceState = { servers: iceServers, fetchedAt: Date.now(), ttl };

	return iceServers;
}

/**
 * How this peer identifies itself to the signaller.
 *
 * A puter token makes it an `authToken` and the peer belongs to that user; a peer
 * token makes it an `anonToken` and the peer is anonymous, with the signaller
 * minting `ANON-*` invite codes for it.
 */
function peerAuth(): [token: string, anon: boolean] {
	if (PUTER_TOKEN) return [PUTER_TOKEN, false];
	if (PEER_TOKEN) return [PEER_TOKEN, true];
	throw new Error("no puter token and no peer token: peers are unavailable");
}

export async function connectToPeer(target: {
	code?: string;
	port?: number;
}): Promise<
	[
		ReadableStream<Uint8Array<ArrayBuffer>>,
		WritableStream<Uint8Array<ArrayBuffer>>,
	]
> {
	let [token, anon] = peerAuth();

	// The connection arrives as *attachments* — a readable and a writable, transferred.
	// That is what makes this op async-only: a stream pair is not something a message body
	// can hold, which is why it used to need a message type of its own.
	let { attachments } = await call<PeerResult<"peer.connect">>(KIND_PEER, {
		op: "peer.connect",
		token,
		anon,
		signaller: await getSignaller(),
		ice: await getIceServers(),
		code: target.code,
		port: target.port,
	});

	return [
		attachments[0] as ReadableStream<Uint8Array<ArrayBuffer>>,
		attachments[1] as WritableStream<Uint8Array<ArrayBuffer>>,
	];
}

export async function hostPeerServer(
	port: number,
	cb: (
		stream: [
			ReadableStream<Uint8Array<ArrayBuffer>>,
			WritableStream<Uint8Array<ArrayBuffer>>,
		]
	) => void
): Promise<{ code: string; close: () => void }> {
	let [token, anon] = peerAuth();

	// One attachment: a port carrying an accepted connection's stream pair per message.
	// A real channel, handed over by a message — which is the distinction the wire draws.
	// The listener is long-lived and pushes at its own rate, so it stays a port; nothing
	// about it is request/response.
	let { value, attachments } = await call<PeerResult<"peer.listen">>(
		KIND_PEER,
		{
			op: "peer.listen",
			token,
			anon,
			port,
			signaller: await getSignaller(),
			ice: await getIceServers(),
		}
	);

	let accepted = attachments[0] as MessagePort;
	accepted.onmessage = (e) => {
		cb([e.data.readable, e.data.writable]);
	};

	return {
		code: value.code,
		close: () => accepted.postMessage({ close: true }),
	};
}
