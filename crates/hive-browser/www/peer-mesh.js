// peer-mesh.js — the browser↔browser direct lane (bn-browser-peer-webrtc-mesh).
//
// WHAT THIS IS, AND WHAT IT IS NOT
//
// A browser node's ONLY transport today is iroh QUIC over a relay WebSocket
// (crates/hive-browser/src/lib.rs `BrowserNode::boot`: `presets::Minimal` +
// `RelayMode::Custom`, and the browser build has no UDP/hole-punching at all —
// iroh cfg's it out). Two admitted browsers CAN already reach each other on
// that path (the accept loop takes any peer on `hive/browser/0`, and an iroh
// relay forwards between two clients connected to it), but every byte crosses
// a fleet relay, and it only works while both tabs happen to share a home
// relay — which the worker's own latency-ordered relay list makes unlikely
// across regions. This lane adds a SECOND data path between browser peers: a
// WebRTC DataChannel that, when ICE succeeds, carries those bytes directly.
//
// It is strictly ADDITIVE and can never become the base:
//   * every capability (serve grants, artifact descriptors, db grants, the
//     trusted-caller set) is derived server-side and delivered over the
//     authenticated HTTPS admission path — WebRTC cannot bootstrap itself;
//   * fleet↔browser traffic (invoke, artifact GET, CrrSync toward fleet peers)
//     is iroh-only: the fleet runs no WebRTC agent;
//   * a DTLS certificate is ephemeral and says nothing about the ed25519
//     EndpointId every grant on this platform is keyed on, so the iroh
//     identity stays the anchor even here (see `envelopeContext`).
// Every consumer therefore treats a direct link as an OPTIMISATION with a
// fallback, never a dependency: `fetchArtifact` returns null and the caller
// fetches from the fleet; `dbPeers()` returns [] and only fleet rounds run.
//
// HOSTING CONSTRAINT (decides the whole shape). `RTCPeerConnection` is
// `[Exposed=Window]` — it does not exist in a SharedWorker or a dedicated
// Worker global scope, so this module CANNOT open a peer connection itself. It
// drives one through a page, exactly like the sqlite DedicatedWorker broker in
// run-node-worker.js: the worker asks a connected page for an agent, the page
// constructs the RTCPeerConnection (see peer-mesh-agent.js) and transfers back
// a MessagePort. Feature-detected via `requestAgent`, never context-sniffed:
// if some engine ever exposes RTCPeerConnection to workers, the host can hand
// back a same-thread port and nothing here changes.
//
// SIGNALLING rides the platform's existing authenticated surface — the host
// supplies `postSignals`, which run-node-worker.js implements as one
// POST to the fleet's signalling mailbox (contract in that file's header).
// POST, never GET: `admin_ingress` forwards mutations to the control-plane
// leader but serves GETs locally against round-robin DNS, so a mailbox read
// over GET would land on an arbitrary node (AGENTS.md, "Round-robin reads vs
// leader-forwarded writes"). Peers are named ONLY by the server-derived
// capability, so a browser can neither enumerate strangers nor address one.

export const MESH_PROTOCOL_VERSION = 1;
// Byte-identical to `MESH_SIG_DOMAIN` in crates/hive-browser/src/lib.rs,
// TRAILING NUL INCLUDED. Exported for documentation and for any out-of-band
// verifier only: this half never builds the signed message, it builds the
// CONTEXT (`envelopeContext`) and the wasm half prepends this domain. Two
// implementations of the prefix is exactly the drift that would make every
// handshake fail verification with nothing to point at.
export const MESH_SIG_DOMAIN = "hive-browser-webrtc-v1\0";
export const MESH_AGENT_PROTOCOL = "hive-mesh-agent/1";
export const MESH_WIRE = "hive-mesh/1";

const HEX64 = /^[0-9a-f]{64}$/;
// `sha-256 AB:CD:...` — the exact shape an SDP a=fingerprint line carries.
const FINGERPRINT_RE = /^[a-z0-9-]{3,16} [0-9A-F]{2}(:[0-9A-F]{2}){15,63}$/;

// DataChannel framing. SCTP messages above ~16 KiB are where cross-browser
// behaviour stops being uniform, so every frame is chunked to that and
// reassembled here; the call cap mirrors the iroh lane's BROWSER_MAX_CRR_FRAME
// (4 MiB) so a CRR round that fits one transport fits the other.
const CHUNK_BYTES = 16 * 1024;
const FRAME_HEADER = 8;
const MAX_CALL_BYTES = 4 * 1024 * 1024;
const MAX_INFLIGHT_CALLS = 8;
const CALL_TIMEOUT_MS = 30_000;

const OP_REQUEST = 1;
const OP_REPLY = 2;
const OP_ERROR = 3;
const KIND_CRR = 0;
const KIND_ARTIFACT = 1;
const FLAG_FINAL = 1;

// Signalling cadence: fast only while a handshake is actually in flight.
const POLL_IDLE_MS = 15_000;
const POLL_ACTIVE_MS = 1_500;
const POLL_MAX_MS = 60_000;
const HANDSHAKE_TIMEOUT_MS = 30_000;
const ENVELOPE_MAX_AGE_MS = 60_000;
const SESSION_RETRY_BASE_MS = 20_000;
const SESSION_RETRY_CAP_MS = 5 * 60_000;
// A peer whose id sorts above ours is the offerer. If it never offers (an
// older worker, or a tab with no page able to host an agent) we offer anyway
// after this long, so one stale side cannot veto the link.
const PASSIVE_GRACE_MS = 45_000;

const nowMs = () => Date.now();
const hex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

/** The canonical string both peers sign and verify. Every field is
 *  charset-restricted, so `|` is an unambiguous separator and no component can
 *  forge a boundary. The FINGERPRINT is the load-bearing member: signing it
 *  binds the ephemeral DTLS identity to the ed25519 EndpointId the server
 *  admitted, which is what stops the signalling mailbox — authenticated, but
 *  still a third party to the peer link — from substituting its own
 *  certificate and sitting in the middle of a "direct" connection. */
export function envelopeContext({ kind, from, to, session, ts, fingerprint }) {
  return `${MESH_WIRE}|${kind}|${from}|${to}|${session}|${ts}|${fingerprint}`;
}

/** Pull the fingerprint out of an SDP. Refuses anything but exactly one
 *  well-formed a=fingerprint line: a session description carrying two
 *  different fingerprints (per-media overrides) is ambiguous about what was
 *  actually signed, and ambiguity here is indistinguishable from an attack. */
export function sdpFingerprint(sdp) {
  const found = new Set();
  for (const line of String(sdp).split(/\r\n|\r|\n/)) {
    if (!line.startsWith("a=fingerprint:")) continue;
    found.add(line.slice("a=fingerprint:".length).trim().toUpperCase().replace(/^([A-Z0-9-]+)/, (m) => m.toLowerCase()));
  }
  if (found.size !== 1) return null;
  const value = [...found][0];
  return FINGERPRINT_RE.test(value) ? value : null;
}

function randomHex(bytes) {
  return hex(crypto.getRandomValues(new Uint8Array(bytes)));
}

function encodeFrame(op, kind, callId, final, chunk) {
  const frame = new Uint8Array(FRAME_HEADER + chunk.length);
  const view = new DataView(frame.buffer);
  frame[0] = op;
  frame[1] = kind;
  frame[2] = final ? FLAG_FINAL : 0;
  frame[3] = 0;
  view.setUint32(4, callId, true);
  frame.set(chunk, FRAME_HEADER);
  return frame;
}

function decodeFrame(bytes) {
  if (!(bytes instanceof Uint8Array) || bytes.length < FRAME_HEADER) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    op: bytes[0],
    kind: bytes[1],
    final: (bytes[2] & FLAG_FINAL) !== 0,
    callId: view.getUint32(4, true),
    chunk: bytes.subarray(FRAME_HEADER),
  };
}

function concat(parts, total) {
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/** Validate the server-derived `mesh` capability block. Malformed is THROWN,
 *  never sanitized: this block is server-derived, so a bad shape is a platform
 *  bug worth surfacing — the caller lands it in status and keeps the relay
 *  path, it never fails an admission. */
export function normalizeMeshCapability(mesh) {
  if (mesh === undefined || mesh === null) return null;
  if (typeof mesh !== "object") throw new Error("capability mesh block is not an object");
  if (mesh.enabled === false) return null;
  const iceServers = (Array.isArray(mesh.ice_servers) ? mesh.ice_servers : [])
    .filter((s) => s && typeof s === "object")
    .map((s) => {
      const urls = (Array.isArray(s.urls) ? s.urls : [s.urls])
        .filter((u) => typeof u === "string" && /^(stun|turn|turns):[^\s"']{1,300}$/.test(u));
      if (!urls.length) return null;
      const entry = { urls };
      if (typeof s.username === "string" && s.username.length <= 256) entry.username = s.username;
      if (typeof s.credential === "string" && s.credential.length <= 512) entry.credential = s.credential;
      return entry;
    })
    .filter(Boolean)
    .slice(0, 8);
  const peers = (Array.isArray(mesh.peers) ? mesh.peers : [])
    .filter((p) => p && typeof p === "object" && HEX64.test(p.endpoint_id || ""))
    .map((p) => ({
      endpointId: p.endpoint_id,
      scope: p.scope === "public" ? "public" : "team",
      project: typeof p.project === "string" ? p.project.slice(0, 128) : "",
      // The peer's SERVER-DERIVED database access. "none" (and anything
      // unrecognised, including absent) means no CRR exchange with this peer
      // at all — absent capability, never a broader one.
      db: p.db === "read_write" || p.db === "read_only" ? p.db : "none",
      // Exact policy digests this peer is authorized to hold. A digest outside
      // this set is never served to it and never requested from it.
      artifacts: (Array.isArray(p.artifacts) ? p.artifacts : []).filter((d) => HEX64.test(d || "")).slice(0, 64),
    }))
    .slice(0, 32);
  return {
    iceServers,
    peers,
    pollMs: Number.isSafeInteger(mesh.signal_poll_ms) ? Math.min(Math.max(mesh.signal_poll_ms, 500), POLL_MAX_MS) : POLL_ACTIVE_MS,
  };
}

export class PeerMesh {
  /**
   * @param opts.selfId        this node's 64-hex EndpointId
   * @param opts.sign          (context) => Promise<hex>  — node.signMeshEnvelope
   * @param opts.verify        (peerId, context, sigHex) => boolean — verifyMeshEnvelope
   * @param opts.postSignals   (body) => Promise<{messages, retry_ms}> — the
   *                           authenticated mailbox round trip (POST only)
   * @param opts.requestAgent  () => Promise<MessagePort> — brokered page agent
   * @param opts.handlers      { artifact(peerId, digest), crr(peerId, bytes) }
   * @param opts.onChange      () => void — status changed
   */
  constructor(opts) {
    this.selfId = opts.selfId;
    this.sign = opts.sign;
    this.verify = opts.verify;
    this.postSignals = opts.postSignals;
    this.requestAgent = opts.requestAgent;
    this.handlers = opts.handlers || {};
    this.onChange = opts.onChange || (() => {});
    this.capability = null;
    this.sessions = new Map(); // peerId -> session
    this.agent = null;
    this.agentSeq = 0;
    this.agentPending = new Map();
    this.agentStarting = null;
    this.outbox = [];
    this.ackSeq = 0;
    this.pollTimer = null;
    this.pollInFlight = false;
    this.pollAgain = false;
    this.closed = false;
    this.unsupported = false; // the fleet has no signalling arm (pre-upgrade)
    this.error = null;
    // `direct` counts SUCCESSFUL channel opens over the lane's whole life —
    // `connected` only ever shows the live count, so a link that came up and
    // dropped leaves no trace in it. That distinction is the answer to the one
    // question this lane exists to answer ("did any byte actually go direct, or
    // did it all fall back to the relay?"), so it is a counter, not a gauge.
    this.stats = { framesIn: 0, framesOut: 0, bytesIn: 0, bytesOut: 0, artifactHits: 0, crrRounds: 0, direct: 0, signalRefused: 0 };
  }

  summary() {
    const peers = [];
    for (const [peerId, session] of this.sessions) {
      peers.push({ peer: peerId, state: session.state, since: session.since, error: session.error });
    }
    return {
      protocol: MESH_PROTOCOL_VERSION,
      state: this.closed ? "stopped" : this.unsupported ? "unsupported" : this.capability ? "running" : "idle",
      known: this.capability ? this.capability.peers.length : 0,
      connected: peers.filter((p) => p.state === "open").length,
      peers,
      direct: this.stats.direct,
      artifactHits: this.stats.artifactHits,
      crrRounds: this.stats.crrRounds,
      bytesIn: this.stats.bytesIn,
      bytesOut: this.stats.bytesOut,
      // Envelopes the fleet mailbox DECLINED to deliver — nearly always a peer
      // that left the authorized set between the capability we hold and the
      // server's live re-derivation, which is normal churn. A number that
      // climbs without any peer ever reaching "open" is the honest signal that
      // this node is addressing peers the server does not agree it may.
      signalRefused: this.stats.signalRefused,
      error: this.error,
    };
  }

  /** Adopt a fresh server-derived capability. Peers that left the set are torn
   *  down immediately — a grant that is gone must not keep a live channel. */
  setCapability(mesh) {
    if (this.closed) return;
    this.capability = mesh;
    if (!mesh || !mesh.peers.length) {
      for (const peerId of [...this.sessions.keys()]) this.dropSession(peerId, "peer left the authorized set");
      // No authorized peer means nobody can address this node either: stop
      // polling entirely rather than heartbeat an empty mailbox forever.
      if (this.pollTimer) clearTimeout(this.pollTimer);
      this.pollTimer = null;
      this.onChange();
      return;
    }
    const authorized = new Set(mesh.peers.map((p) => p.endpointId));
    for (const peerId of [...this.sessions.keys()]) {
      if (!authorized.has(peerId)) this.dropSession(peerId, "peer left the authorized set");
    }
    // A fresh capability is also the moment a previously-refused mailbox is
    // worth re-checking: the fleet may have rolled forward since.
    this.unsupported = false;
    this.schedulePoll(0);
    this.onChange();
  }

  peer(peerId) {
    if (!this.capability) return null;
    return this.capability.peers.find((p) => p.endpointId === peerId) || null;
  }

  /** Peers authorized to exchange CRR rounds for this project, shaped as
   *  sync-client peer entries with a DIRECT transport. Only peers with a live
   *  open channel are returned: a peer that has not connected costs the caller
   *  nothing, because the fleet rounds it already runs cover convergence. */
  dbPeers(project) {
    const out = [];
    if (!this.capability) return out;
    for (const peer of this.capability.peers) {
      if (peer.db === "none") continue;
      if (project && peer.project && peer.project !== project) continue;
      const session = this.sessions.get(peer.endpointId);
      if (!session || session.state !== "open") continue;
      out.push({
        endpoint_id: peer.endpointId,
        addr_json: "",
        direct: true,
        access: peer.db,
        send: (bytes) => this.call(peer.endpointId, KIND_CRR, bytes),
      });
    }
    return out;
  }

  /** Ask an authorized peer for a content-addressed artifact. Returns null on
   *  any miss/failure — the caller then fetches from the fleet exactly as
   *  before. The bytes are NOT trusted here: the caller re-verifies size +
   *  BLAKE3 against the descriptor (WorkerFunctionRuntime.pin), which is the
   *  entire reason a peer is allowed to supply them at all. */
  async fetchArtifact(digest, maxBytes) {
    if (this.closed || !this.capability || !HEX64.test(digest || "")) return null;
    for (const peer of this.capability.peers) {
      if (!peer.artifacts.includes(digest)) continue;
      const session = this.sessions.get(peer.endpointId);
      if (!session || session.state !== "open") continue;
      try {
        const bytes = await this.call(peer.endpointId, KIND_ARTIFACT, new TextEncoder().encode(digest));
        if (!bytes || !bytes.length) continue;
        if (Number.isSafeInteger(maxBytes) && bytes.length !== maxBytes) continue;
        this.stats.artifactHits += 1;
        this.onChange();
        return bytes;
      } catch {
        /* try the next peer; the fleet path is always the floor */
      }
    }
    return null;
  }

  // ---- signalling ---------------------------------------------------------

  schedulePoll(delayMs) {
    if (this.closed || this.unsupported) return;
    if (this.pollTimer) clearTimeout(this.pollTimer);
    const base = delayMs !== undefined ? delayMs : this.pending() ? this.capability?.pollMs ?? POLL_ACTIVE_MS : POLL_IDLE_MS;
    this.pollTimer = setTimeout(() => this.poll(), base);
  }

  /** Is there anything the mailbox could be holding for us RIGHT NOW?
   *
   *  Note the passive case, which is the whole reason this is not simply "am I
   *  mid-handshake": the peer whose id sorts LOWER is the offerer, so its
   *  partner sits in `idle` with an empty outbox and nothing negotiating —
   *  measured against the real state machine, that made the passive side drop
   *  to the 15s idle cadence and leave an already-delivered offer unread until
   *  the offerer's 30s handshake timeout had nearly expired. Any authorized
   *  peer that is not connected and not inside its retry backoff means an
   *  envelope may be waiting, so the fast cadence applies to BOTH roles. */
  pending() {
    if (this.outbox.length) return true;
    const now = nowMs();
    for (const session of this.sessions.values()) {
      if (session.state === "open") continue;
      if (session.state === "offering" || session.state === "answering") return true;
      if (!session.retryAfter || now >= session.retryAfter) return true;
    }
    return false;
  }

  queue(envelope) {
    if (this.outbox.length >= 64) this.outbox.shift();
    this.outbox.push(envelope);
    // An offer produced DURING a poll (openPending's handshake work is async
    // and outlives the round trip that started it) must not wait out a whole
    // idle interval — the peer is sitting there waiting for it.
    if (this.pollInFlight) this.pollAgain = true;
    else this.schedulePoll(0);
  }

  async poll() {
    if (this.closed || this.unsupported || this.pollInFlight) return;
    this.pollInFlight = true;
    const sending = this.outbox.splice(0, 16);
    try {
      this.expireSessions();
      this.openPending();
      const reply = await this.postSignals({
        protocol_version: MESH_PROTOCOL_VERSION,
        ack_seq: this.ackSeq,
        send: sending.map((e) => ({ to: e.to, kind: e.kind, payload: JSON.stringify(e.payload) })),
      });
      this.error = null;
      if (Number.isSafeInteger(reply && reply.refused) && reply.refused > 0) {
        this.stats.signalRefused += reply.refused;
      }
      const messages = Array.isArray(reply && reply.messages) ? reply.messages : [];
      for (const message of messages) {
        if (Number.isSafeInteger(message.seq) && message.seq > this.ackSeq) this.ackSeq = message.seq;
        try {
          await this.onEnvelope(message);
        } catch (error) {
          // One bad envelope never stops the mailbox drain.
          this.noteSession(message.from, (s) => { s.error = String(error?.message ?? error).slice(0, 200); });
        }
      }
      // The mailbox delivers a bounded slice per round trip and says so. A
      // backed-up inbox (both peers handshaking at once, or a tab returning
      // from suspension) must drain NOW rather than one slice per poll
      // interval — at the idle cadence that is 15s per slice, long enough for
      // the offerer's handshake timeout to fire on envelopes already sitting
      // here.
      if (reply && reply.more === true) this.pollAgain = true;
      this.onChange();
    } catch (error) {
      // A fleet with no signalling arm yet answers 404/405/501. That is a
      // rollout state, not a failure: the lane goes dormant, the relay path is
      // untouched, and the next admission re-checks (setCapability clears it).
      const status = error && typeof error === "object" ? error.status : null;
      if (status === 404 || status === 405 || status === 501) {
        this.unsupported = true;
        this.error = "this fleet has no browser signalling arm yet — peer links stay on the relay path";
      } else {
        this.error = String(error?.message ?? error).slice(0, 200);
        // Un-sent envelopes go back to the FRONT: dropping an offer silently
        // strands the peer waiting for it until the handshake times out.
        this.outbox = [...sending, ...this.outbox].slice(0, 64);
      }
      this.onChange();
    } finally {
      this.pollInFlight = false;
      const immediate = this.pollAgain;
      this.pollAgain = false;
      this.schedulePoll(immediate ? 0 : undefined);
    }
  }

  async onEnvelope(message) {
    const from = message && message.from;
    if (!HEX64.test(from || "")) return;
    const peer = this.peer(from);
    // Not in the server-derived set = not addressable. The mailbox is supposed
    // to enforce this too; this is the client half of the same rule.
    if (!peer) return;
    let payload;
    try {
      payload = typeof message.payload === "string" ? JSON.parse(message.payload) : message.payload;
    } catch {
      return;
    }
    if (!payload || typeof payload !== "object") return;
    if (payload.v !== MESH_PROTOCOL_VERSION) return;
    if (payload.to !== this.selfId || payload.from !== from) return;
    if (!/^[0-9a-f]{32}$/.test(payload.session || "")) return;
    const ts = Number(payload.ts);
    if (!Number.isSafeInteger(ts) || Math.abs(nowMs() - ts) > ENVELOPE_MAX_AGE_MS) return;

    if (message.kind === "bye") {
      this.dropSession(from, "peer closed the link");
      return;
    }
    if (message.kind === "ice") {
      const session = this.sessions.get(from);
      if (!session || session.id !== payload.session || !session.agentReady) return;
      if (typeof payload.candidate !== "string" || payload.candidate.length > 1024) return;
      await this.agentCall("addCandidate", { peer: from, candidate: payload.candidate, mid: payload.mid ?? null });
      return;
    }
    if (message.kind !== "offer" && message.kind !== "answer") return;

    const fingerprint = typeof payload.fingerprint === "string" ? payload.fingerprint : "";
    if (!FINGERPRINT_RE.test(fingerprint)) return;
    const context = envelopeContext({
      kind: message.kind, from, to: this.selfId, session: payload.session, ts, fingerprint,
    });
    if (!this.verify(from, context, String(payload.sig || ""))) {
      throw new Error("peer signalling envelope failed ed25519 verification");
    }
    if (typeof payload.sdp !== "string" || payload.sdp.length > 64 * 1024) return;
    // The signed fingerprint must be the one actually inside the SDP — the
    // signature is worthless if the description carries a different cert.
    if (sdpFingerprint(payload.sdp) !== fingerprint) {
      throw new Error("peer SDP fingerprint does not match the signed one");
    }

    if (message.kind === "offer") await this.acceptOffer(from, payload, fingerprint);
    else await this.acceptAnswer(from, payload, fingerprint);
  }

  // ---- session lifecycle --------------------------------------------------

  noteSession(peerId, mutate) {
    const session = this.sessions.get(peerId);
    if (session) mutate(session);
  }

  session(peerId, create) {
    let session = this.sessions.get(peerId);
    if (!session && create) {
      session = {
        peerId, id: null, state: "idle", since: nowMs(), error: null, agentReady: false,
        calls: new Map(), nextCall: 1, inbound: new Map(), retryAfter: 0, retryMs: SESSION_RETRY_BASE_MS,
        knownAt: nowMs(),
      };
      this.sessions.set(peerId, session);
    }
    return session;
  }

  dropSession(peerId, reason) {
    const session = this.sessions.get(peerId);
    if (!session) return;
    for (const call of session.calls.values()) call.reject(new Error(reason || "peer link closed"));
    session.calls.clear();
    session.inbound.clear();
    this.sessions.delete(peerId);
    if (this.agent) {
      this.agentCall("close", { peer: peerId }).catch(() => {});
    }
    this.onChange();
  }

  expireSessions() {
    const now = nowMs();
    for (const [peerId, session] of [...this.sessions]) {
      if ((session.state === "offering" || session.state === "answering") && now - session.since > HANDSHAKE_TIMEOUT_MS) {
        session.state = "failed";
        session.error = "handshake timed out";
        session.retryAfter = now + session.retryMs;
        session.retryMs = Math.min(session.retryMs * 2, SESSION_RETRY_CAP_MS);
        session.id = null;
        session.agentReady = false;
        if (this.agent) this.agentCall("close", { peer: peerId }).catch(() => {});
      }
    }
  }

  /** Start handshakes with every authorized peer we should be initiating to.
   *  Deterministic roles by id comparison (no glare protocol needed): the
   *  LOWER id offers. A passive side whose partner never offers takes over
   *  after PASSIVE_GRACE_MS, so one stale peer cannot veto the link. */
  openPending() {
    if (!this.capability) return;
    const now = nowMs();
    for (const peer of this.capability.peers) {
      const session = this.session(peer.endpointId, true);
      if (session.state === "open" || session.state === "offering" || session.state === "answering") continue;
      if (session.retryAfter && now < session.retryAfter) continue;
      const weOffer = this.selfId < peer.endpointId || now - session.knownAt > PASSIVE_GRACE_MS;
      if (!weOffer) continue;
      this.startOffer(peer.endpointId).catch((error) => {
        session.state = "failed";
        session.error = String(error?.message ?? error).slice(0, 200);
        session.retryAfter = nowMs() + session.retryMs;
        session.retryMs = Math.min(session.retryMs * 2, SESSION_RETRY_CAP_MS);
        this.onChange();
      });
    }
  }

  async startOffer(peerId) {
    const session = this.session(peerId, true);
    session.id = randomHex(16);
    session.state = "offering";
    session.since = nowMs();
    session.error = null;
    await this.ensureAgent();
    const result = await this.agentCall("createOffer", {
      peer: peerId, session: session.id, iceServers: this.capability?.iceServers ?? [],
    });
    session.agentReady = true;
    await this.emitDescription("offer", peerId, session.id, result);
  }

  async acceptOffer(peerId, payload, fingerprint) {
    const session = this.session(peerId, true);
    // Glare: both sides offered. The LOWER id's offer wins — the higher-id
    // side abandons its own and answers, so the pair converges on one session
    // instead of tearing both down and retrying forever.
    if (session.state === "offering" && this.selfId < peerId) return;
    session.id = payload.session;
    session.state = "answering";
    session.since = nowMs();
    session.error = null;
    await this.ensureAgent();
    const result = await this.agentCall("acceptOffer", {
      peer: peerId, session: session.id, sdp: payload.sdp, expectFingerprint: fingerprint,
      iceServers: this.capability?.iceServers ?? [],
    });
    session.agentReady = true;
    await this.emitDescription("answer", peerId, session.id, result);
  }

  async acceptAnswer(peerId, payload, fingerprint) {
    const session = this.sessions.get(peerId);
    if (!session || session.id !== payload.session || session.state !== "offering") return;
    await this.agentCall("acceptAnswer", {
      peer: peerId, session: session.id, sdp: payload.sdp, expectFingerprint: fingerprint,
    });
  }

  async emitDescription(kind, peerId, sessionId, result) {
    // The agent round trip (broker + ICE gathering) can outlive the session it
    // was started for — `expireSessions` may have already failed it. Publishing
    // that description anyway invites the peer to answer into a session id this
    // side no longer holds, which it would then silently ignore.
    const live = this.sessions.get(peerId);
    if (!live || live.id !== sessionId) return;
    const sdp = result && typeof result.sdp === "string" ? result.sdp : "";
    const fingerprint = sdpFingerprint(sdp);
    if (!fingerprint) throw new Error("local session description carries no usable fingerprint");
    const ts = nowMs();
    const context = envelopeContext({ kind, from: this.selfId, to: peerId, session: sessionId, ts, fingerprint });
    const sig = await this.sign(context);
    this.queue({
      to: peerId, kind,
      payload: { v: MESH_PROTOCOL_VERSION, from: this.selfId, to: peerId, session: sessionId, ts, fingerprint, sdp, sig },
    });
  }

  // ---- the page-hosted RTCPeerConnection agent ----------------------------

  async ensureAgent() {
    if (this.agent) return this.agent;
    if (!this.agentStarting) {
      this.agentStarting = (async () => {
        const port = await this.requestAgent();
        if (this.closed) {
          try { port.close(); } catch { /* already gone */ }
          throw new Error("peer mesh closed while acquiring its agent");
        }
        port.onmessage = (event) => this.onAgentMessage(event.data);
        if (typeof port.start === "function") port.start();
        this.agent = port;
        return port;
      })().finally(() => { this.agentStarting = null; });
    }
    return this.agentStarting;
  }

  /** The page hosting the agent went away — every PeerConnection died with it.
   *  Sessions are failed (not deleted) so their retry backoff still applies,
   *  and the next handshake re-brokers from a surviving page. */
  agentLost(reason) {
    this.agent = null;
    for (const pending of this.agentPending.values()) pending.reject(new Error(reason || "peer-mesh agent lost"));
    this.agentPending.clear();
    for (const [peerId, session] of this.sessions) {
      if (session.state === "idle") continue;
      this.onPeerState(peerId, "failed", reason || "peer-mesh agent lost");
    }
    this.onChange();
  }

  agentCall(op, fields, transfer) {
    if (!this.agent) return Promise.reject(new Error("no peer-mesh agent is attached"));
    const id = ++this.agentSeq;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.agentPending.delete(id)) reject(new Error(`peer-mesh agent op '${op}' timed out`));
      }, 20_000);
      this.agentPending.set(id, {
        resolve: (value) => { clearTimeout(timer); resolve(value); },
        reject: (error) => { clearTimeout(timer); reject(error); },
      });
      try {
        this.agent.postMessage({ proto: MESH_AGENT_PROTOCOL, id, op, ...fields }, transfer || []);
      } catch (error) {
        clearTimeout(timer);
        this.agentPending.delete(id);
        reject(error);
      }
    });
  }

  onAgentMessage(message) {
    if (!message || message.proto !== MESH_AGENT_PROTOCOL) return;
    if (message.event === "state") return this.onPeerState(message.peer, message.state, message.detail);
    if (message.event === "frame") return this.onFrame(message.peer, new Uint8Array(message.frame));
    if (message.event === "candidate") return this.onLocalCandidate(message.peer, message.candidate, message.mid);
    const pending = this.agentPending.get(message.id);
    if (!pending) return;
    this.agentPending.delete(message.id);
    if (message.error) pending.reject(new Error(String(message.error).slice(0, 200)));
    else pending.resolve(message);
  }

  onPeerState(peerId, state, detail) {
    const session = this.sessions.get(peerId);
    if (!session) return;
    if (state === "open") {
      session.state = "open";
      session.since = nowMs();
      session.error = null;
      session.retryMs = SESSION_RETRY_BASE_MS;
      session.retryAfter = 0;
      this.stats.direct += 1;
    } else if (state === "closed" || state === "failed") {
      for (const call of session.calls.values()) call.reject(new Error(`peer link ${state}`));
      session.calls.clear();
      session.inbound.clear();
      session.state = state;
      session.error = detail ? String(detail).slice(0, 200) : session.error;
      session.id = null;
      session.agentReady = false;
      session.retryAfter = nowMs() + session.retryMs;
      session.retryMs = Math.min(session.retryMs * 2, SESSION_RETRY_CAP_MS);
    }
    this.onChange();
    this.schedulePoll();
  }

  async onLocalCandidate(peerId, candidate, mid) {
    const session = this.sessions.get(peerId);
    if (!session || !session.id || typeof candidate !== "string" || candidate.length > 1024) return;
    // Trickle stragglers only: the offer/answer already carried whatever was
    // gathered before the agent's bounded deadline. Candidates are not signed
    // — a forged one can only produce a failed connection, because DTLS still
    // has to match the SIGNED fingerprint before any byte flows.
    this.queue({
      to: peerId, kind: "ice",
      payload: { v: MESH_PROTOCOL_VERSION, from: this.selfId, to: peerId, session: session.id, ts: nowMs(), candidate, mid: mid ?? null },
    });
  }

  // ---- calls over the DataChannel ----------------------------------------

  async call(peerId, kind, payload) {
    const session = this.sessions.get(peerId);
    if (!session || session.state !== "open") throw new Error("no direct link to this peer");
    if (session.calls.size >= MAX_INFLIGHT_CALLS) throw new Error("peer call limit reached");
    if (payload.length > MAX_CALL_BYTES) throw new Error("peer call payload exceeds the frame cap");
    const callId = session.nextCall;
    session.nextCall = (session.nextCall + 1) % 0xffffffff || 1;
    const pending = {};
    const promise = new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (session.calls.delete(callId)) reject(new Error("peer call timed out"));
      }, CALL_TIMEOUT_MS);
      pending.resolve = (value) => { clearTimeout(timer); resolve(value); };
      pending.reject = (error) => { clearTimeout(timer); reject(error); };
    });
    session.calls.set(callId, pending);
    try {
      await this.sendCall(peerId, OP_REQUEST, kind, callId, payload);
    } catch (error) {
      session.calls.delete(callId);
      throw error;
    }
    return promise;
  }

  async sendCall(peerId, op, kind, callId, payload) {
    let at = 0;
    do {
      const end = Math.min(at + (CHUNK_BYTES - FRAME_HEADER), payload.length);
      const chunk = payload.subarray(at, end);
      const frame = encodeFrame(op, kind, callId, end >= payload.length, chunk);
      // Read the size BEFORE the post: transferring detaches the buffer, after
      // which `frame.length` reads 0 and the byte counter silently flatlines.
      const size = frame.length;
      // `frame.buffer` is freshly allocated per chunk, so it is TRANSFERRED to
      // the page agent rather than structured-clone-copied. Each await is the
      // backpressure point: the agent resolves a send only once the channel's
      // buffered amount has drained under its threshold.
      await this.agentCall("send", { peer: peerId, frame: frame.buffer }, [frame.buffer]);
      this.stats.framesOut += 1;
      this.stats.bytesOut += size;
      at = end;
    } while (at < payload.length);
  }

  onFrame(peerId, bytes) {
    const session = this.sessions.get(peerId);
    if (!session) return;
    const frame = decodeFrame(bytes);
    if (!frame) return;
    this.stats.framesIn += 1;
    this.stats.bytesIn += bytes.length;
    const key = `${frame.op === OP_REQUEST ? "q" : "r"}:${frame.callId}`;
    let entry = session.inbound.get(key);
    if (!entry) {
      if (session.inbound.size >= MAX_INFLIGHT_CALLS * 2) return; // bounded reassembly
      entry = { parts: [], total: 0 };
      session.inbound.set(key, entry);
    }
    if (entry.total + frame.chunk.length > MAX_CALL_BYTES) {
      session.inbound.delete(key);
      return;
    }
    // The view, not a copy: each inbound frame owns its own ArrayBuffer (the
    // agent transfers one per message), so retaining the view retains the same
    // bytes and `concat` below does the single copy that assembly needs.
    entry.parts.push(frame.chunk);
    entry.total += frame.chunk.length;
    if (!frame.final) return;
    session.inbound.delete(key);
    const payload = concat(entry.parts, entry.total);
    if (frame.op === OP_REQUEST) this.serve(peerId, frame.kind, frame.callId, payload);
    else {
      const pending = session.calls.get(frame.callId);
      if (!pending) return;
      session.calls.delete(frame.callId);
      if (frame.op === OP_ERROR) pending.reject(new Error(new TextDecoder().decode(payload).slice(0, 200)));
      else pending.resolve(payload);
    }
  }

  async serve(peerId, kind, callId, payload) {
    // Authorization is checked TWICE on purpose: the peer must be in the
    // server-derived set here, and the host handler re-checks the exact grant
    // (this digest, this db access) before producing a byte.
    const peer = this.peer(peerId);
    try {
      if (!peer) throw new Error("forbidden");
      let reply;
      if (kind === KIND_ARTIFACT) {
        const digest = new TextDecoder().decode(payload);
        if (!HEX64.test(digest)) throw new Error("malformed digest");
        if (!peer.artifacts.includes(digest)) throw new Error("forbidden");
        reply = this.handlers.artifact ? await this.handlers.artifact(peerId, digest) : null;
        if (!reply) throw new Error("not held");
      } else if (kind === KIND_CRR) {
        if (peer.db === "none") throw new Error("forbidden");
        if (!this.handlers.crr) throw new Error("no db lane");
        reply = await this.handlers.crr(peerId, payload, peer.db);
        this.stats.crrRounds += 1;
      } else {
        throw new Error("unknown kind");
      }
      await this.sendCall(peerId, OP_REPLY, kind, callId, reply);
    } catch (error) {
      const message = new TextEncoder().encode(String(error?.message ?? error).slice(0, 200));
      try {
        await this.sendCall(peerId, OP_ERROR, kind, callId, message);
      } catch {
        /* link already gone */
      }
    }
  }

  async stop() {
    if (this.closed) return;
    this.closed = true;
    if (this.pollTimer) clearTimeout(this.pollTimer);
    this.pollTimer = null;
    for (const peerId of [...this.sessions.keys()]) {
      // Best-effort courtesy notice so the peer tears down now instead of
      // waiting out its own handshake/idle timers.
      this.outbox.push({ to: peerId, kind: "bye", payload: { v: MESH_PROTOCOL_VERSION, from: this.selfId, to: peerId, session: this.sessions.get(peerId)?.id || "0".repeat(32), ts: nowMs() } });
      this.dropSession(peerId, "peer mesh stopped");
    }
    const farewell = this.outbox.splice(0, 16);
    if (farewell.length && !this.unsupported) {
      try {
        await this.postSignals({
          protocol_version: MESH_PROTOCOL_VERSION, ack_seq: this.ackSeq,
          send: farewell.map((e) => ({ to: e.to, kind: e.kind, payload: JSON.stringify(e.payload) })),
        });
      } catch {
        /* the peer's own handshake timeout covers this */
      }
    }
    if (this.agent) {
      try { this.agent.postMessage({ proto: MESH_AGENT_PROTOCOL, op: "shutdown" }); } catch { /* gone */ }
      try { this.agent.close(); } catch { /* gone */ }
      this.agent = null;
    }
    for (const pending of this.agentPending.values()) pending.reject(new Error("peer mesh stopped"));
    this.agentPending.clear();
    this.onChange();
  }
}
