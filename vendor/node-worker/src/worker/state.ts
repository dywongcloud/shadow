// Deliberately importless. Both `puter.ts` and the fs layer read this, so giving it
// a dependency pulls whatever that is into everything — importing `node/path` here
// to normalize `CWD` added a dozen cycle paths through the node subgraph. The
// normalization it was for lives in `normalizePath` instead, which anchors its
// resolve at "/" and so tolerates a relative value here.

export let PUTER_TOKEN: string | undefined;
export let CWD: string = "/";

// The anonymous network, when there is no puter token to mint it with. See
// `NodeNetInit` in ../protocol.ts.
export let WISP_URL: string | undefined;
export let RELAY_TOKEN: string | undefined;
export let PEER_TOKEN: string | undefined;

export function setPuterToken(token: string | undefined) {
	PUTER_TOKEN = token || undefined;
}

export function setNet(net: {
	wispUrl?: string;
	relayToken?: string;
	peerToken?: string;
}) {
	WISP_URL = net.wispUrl || undefined;
	RELAY_TOKEN = net.relayToken || undefined;
	PEER_TOKEN = net.peerToken || undefined;
}

export function setPuterCWD(cwd: string) {
	CWD = cwd;
}
