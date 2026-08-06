// peer-mesh-agent.js — the PAGE half of the browser↔browser direct lane
// (bn-browser-peer-webrtc-mesh). Owns the RTCPeerConnections; owns no policy.
//
// WHY THIS FILE EXISTS AT ALL. `RTCPeerConnection` is `[Exposed=Window]`: it
// does not exist in a SharedWorker or a dedicated Worker global scope, so the
// run-node worker — which owns the node, the admission and every capability —
// structurally cannot open a peer connection itself. It brokers one through a
// connected page, exactly like the sqlite DedicatedWorker broker already in
// run-node-worker.js. The page-side contract is three messages:
//
//   worker -> page : { type: "meshPeerRequest", requestId }
//   page  -> worker: { type: "meshPeerPort", requestId } + transfer [port]
//   worker -> page : { type: "meshPeerDone", requestId }
//
// and the page's whole job on receipt is:
//
//   const channel = new MessageChannel();
//   startMeshAgent(channel.port1);
//   workerPort.postMessage({ type: "meshPeerPort", requestId }, [channel.port2]);
//
// plus stopping the returned agent on `meshPeerDone` and on its own unload —
// an orphaned agent holds live PeerConnections and ICE candidates after the
// node that authorized them is gone.
//
// THE PAGE IS NOT TRUSTED WITH POLICY. It never learns a peer id it was not
// handed, never chooses an ICE server (the worker passes the server-derived
// set), and never sees a capability, an artifact byte's provenance or a db
// grant. It also re-checks the one thing it is uniquely positioned to enforce:
// that the fingerprint inside a remote SDP is the fingerprint the worker
// verified an ed25519 signature over, BEFORE the description reaches the
// browser's DTLS stack. Data frames pass through opaquely.

// The fingerprint parser is imported, never re-implemented: the worker signs
// what IT extracted from an SDP and this file refuses what does not match, so
// two independent parsers would be a security-relevant drift waiting to
// happen (the same one-contract-two-implementations discipline the policy
// digest and `val_payload_bytes` carry elsewhere in this tree).
import { sdpFingerprint } from "./peer-mesh.js";

const PROTO = "hive-mesh-agent/1";
const CHANNEL_LABEL = "hive-mesh";
// Both sides open the SAME pre-negotiated channel, so there is no
// `ondatachannel` race and no in-band negotiation to get wrong.
const CHANNEL_ID = 0;
// ICE gathering can run for many seconds chasing relay candidates. The
// offer/answer goes out with whatever is ready at this deadline and stragglers
// trickle after (the signalling mailbox carries `ice` envelopes) — waiting for
// "complete" would add that whole tail to every handshake.
const GATHER_DEADLINE_MS = 2_500;
const SEND_BUFFER_HIGH = 1 << 20;
const SEND_BUFFER_LOW = 256 << 10;

/** Start one agent bound to `port` (the worker's end of a MessageChannel).
 *  Returns { stop() } — call it on `meshPeerDone` and on page unload. */
export function startMeshAgent(port) {
  const peers = new Map(); // peerId -> { pc, channel, ready }
  let stopped = false;

  const emit = (message, transfer) => {
    if (stopped) return;
    try {
      port.postMessage({ proto: PROTO, ...message }, transfer || []);
    } catch {
      /* worker gone; stop() follows from its own teardown */
    }
  };

  function teardown(peerId) {
    const entry = peers.get(peerId);
    if (!entry) return;
    peers.delete(peerId);
    try { entry.channel?.close(); } catch { /* already closed */ }
    try { entry.pc.close(); } catch { /* already closed */ }
  }

  function create(peerId, iceServers) {
    teardown(peerId);
    const pc = new RTCPeerConnection({
      iceServers: Array.isArray(iceServers) ? iceServers : [],
      // Data channels only — no tracks, no transceivers, no media permissions.
      bundlePolicy: "max-bundle",
    });
    const channel = pc.createDataChannel(CHANNEL_LABEL, { negotiated: true, id: CHANNEL_ID, ordered: true });
    channel.binaryType = "arraybuffer";
    const entry = { pc, channel, ready: false, gathered: false };
    peers.set(peerId, entry);

    channel.onopen = () => {
      entry.ready = true;
      emit({ event: "state", peer: peerId, state: "open" });
    };
    channel.onclose = () => {
      entry.ready = false;
      emit({ event: "state", peer: peerId, state: "closed" });
    };
    channel.onerror = (event) => {
      emit({ event: "state", peer: peerId, state: "failed", detail: String(event?.error?.message || "data channel error") });
    };
    channel.onmessage = (event) => {
      const data = event.data;
      if (!(data instanceof ArrayBuffer)) return;
      emit({ event: "frame", peer: peerId, frame: data }, [data]);
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === "failed" || pc.connectionState === "closed") {
        emit({ event: "state", peer: peerId, state: "failed", detail: `connection ${pc.connectionState}` });
      }
    };
    pc.onicecandidate = (event) => {
      // Only stragglers matter: everything gathered before the deadline is
      // already inside the description that was signed and sent.
      if (!event.candidate || !entry.gathered) return;
      emit({ event: "candidate", peer: peerId, candidate: event.candidate.candidate, mid: event.candidate.sdpMid ?? null });
    };
    return entry;
  }

  async function gatherBounded(pc) {
    if (pc.iceGatheringState === "complete") return;
    await new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        pc.removeEventListener("icegatheringstatechange", check);
        resolve();
      };
      const check = () => {
        if (pc.iceGatheringState === "complete") done();
      };
      const timer = setTimeout(done, GATHER_DEADLINE_MS);
      pc.addEventListener("icegatheringstatechange", check);
    });
  }

  async function drain(entry) {
    if (entry.channel.bufferedAmount <= SEND_BUFFER_HIGH) return;
    entry.channel.bufferedAmountLowThreshold = SEND_BUFFER_LOW;
    await new Promise((resolve) => {
      const onLow = () => {
        entry.channel.removeEventListener("bufferedamountlow", onLow);
        resolve();
      };
      entry.channel.addEventListener("bufferedamountlow", onLow);
      // A closed channel never fires the event; the send below then throws,
      // which is the honest failure the worker's call timeout expects.
      setTimeout(onLow, 5_000);
    });
  }

  async function handle(message) {
    const peerId = message.peer;
    switch (message.op) {
      case "createOffer": {
        const entry = create(peerId, message.iceServers);
        const offer = await entry.pc.createOffer();
        await entry.pc.setLocalDescription(offer);
        await gatherBounded(entry.pc);
        entry.gathered = true;
        const sdp = entry.pc.localDescription?.sdp || offer.sdp || "";
        return { sdp, fingerprint: sdpFingerprint(sdp) };
      }
      case "acceptOffer": {
        if (sdpFingerprint(message.sdp) !== message.expectFingerprint) {
          throw new Error("remote offer fingerprint does not match the verified one");
        }
        const entry = create(peerId, message.iceServers);
        await entry.pc.setRemoteDescription({ type: "offer", sdp: message.sdp });
        const answer = await entry.pc.createAnswer();
        await entry.pc.setLocalDescription(answer);
        await gatherBounded(entry.pc);
        entry.gathered = true;
        const sdp = entry.pc.localDescription?.sdp || answer.sdp || "";
        return { sdp, fingerprint: sdpFingerprint(sdp) };
      }
      case "acceptAnswer": {
        const entry = peers.get(peerId);
        if (!entry) throw new Error("no pending peer connection");
        if (sdpFingerprint(message.sdp) !== message.expectFingerprint) {
          throw new Error("remote answer fingerprint does not match the verified one");
        }
        await entry.pc.setRemoteDescription({ type: "answer", sdp: message.sdp });
        return {};
      }
      case "addCandidate": {
        const entry = peers.get(peerId);
        if (!entry) return {};
        try {
          await entry.pc.addIceCandidate({ candidate: message.candidate, sdpMid: message.mid ?? undefined });
        } catch {
          // A candidate for a description that has moved on is normal noise,
          // never a session failure — DTLS still gates every byte.
        }
        return {};
      }
      case "send": {
        const entry = peers.get(peerId);
        if (!entry || !entry.ready) throw new Error("peer channel is not open");
        await drain(entry);
        entry.channel.send(message.frame);
        return {};
      }
      case "close": {
        teardown(peerId);
        return {};
      }
      default:
        throw new Error(`unknown peer-mesh agent op ${JSON.stringify(message.op)}`);
    }
  }

  port.onmessage = (event) => {
    const message = event.data;
    if (!message || message.proto !== PROTO) return;
    if (message.op === "shutdown") return stop();
    handle(message).then(
      (result) => emit({ id: message.id, ...result }),
      (error) => emit({ id: message.id, error: String(error?.message ?? error).slice(0, 200) }),
    );
  };
  if (typeof port.start === "function") port.start();

  function stop() {
    if (stopped) return;
    stopped = true;
    for (const peerId of [...peers.keys()]) teardown(peerId);
    try { port.close(); } catch { /* already closed */ }
  }

  return { stop };
}

/** True when this context can actually host peer connections. The worker calls
 *  nothing here — it feature-detects by asking a page for an agent and honestly
 *  reporting the lane dormant if no page answers. */
export function meshAgentSupported() {
  return typeof RTCPeerConnection === "function";
}
