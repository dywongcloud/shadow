// The peer op set: outbound connections and inbound listeners over WebRTC.
//
// Both ops answer with a *handle* rather than a value — a stream pair for a connection, a
// port for a listener — so both are async only. That is the whole reason attachments
// exist: a peer connection is not something an XHR body can carry, and pretending
// otherwise would mean a second protocol for the two ops that need one.

/** One peer operation. Worker → page. */
export type PeerCall =
	/**
	 * Reply attaches `{readable, writable}` for the connection.
	 *
	 * Two ways to name the far end, and exactly one of them is set. `code` is an invite code,
	 * which is how an *authenticated* server is reached. `port` is the other address the
	 * signaller already matches on — it registers a server under `(credential, port)`, so a
	 * client holding the same anonToken can dial the port directly and never see a code.
	 *
	 * Only `code` existed here, which left this side able to reach a server it could itself
	 * have started but not to name it the way the server was registered.
	 */
	| {
			op: "peer.connect";
			token: string;
			code?: string;
			port?: number;
			signaller: string;
			ice: RTCIceServer[];
			/** `token` is an `anonToken` rather than a puter `authToken`. */
			anon?: boolean;
	  }
	/**
	 * Reply attaches a `MessagePort` carrying one `{readable, writable}` per accepted
	 * connection, and answers with the code peers dial.
	 */
	| {
			op: "peer.listen";
			token: string;
			port: number;
			signaller: string;
			ice: RTCIceServer[];
			anon?: boolean;
	  };

export interface PeerResults {
	"peer.connect": null;
	"peer.listen": { code: string };
}

export type PeerOpName = PeerCall["op"];
export type PeerResult<K extends PeerOpName> = PeerResults[K];
