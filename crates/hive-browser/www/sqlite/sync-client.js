// sync-client.js — the page-side glue of the browser↔fleet CRR exchange
// (bn-browser-fleet-crr-exchange). It ties the two halves together:
//
//   * the sqlite DedicatedWorker (sqlite-worker.js) owns the OPFS replica and
//     speaks HCB1 batches via the sync-state/sync-export/sync-apply envelope
//     ops — CRR semantics live there and in hive-crsql;
//   * the BrowserNode wasm owns the iroh transport — crrSyncOn() carries one
//     encoded Op::CrrSync request to a fleet peer and returns the reply.
//
// This file owns ONLY the session loop: which peers to dial (the admission
// capability's server-derived sync_peers — never page input), how push
// batches are chunked under the wire frame cap, how the multi-round loop
// terminates, and where the push cursor (the fleet's acknowledged watermarks
// per site) is persisted so a tab reload resumes incrementally instead of
// re-exporting from zero (docs/browser-db-contract.md §5 — the watermark
// resume is the whole point of keeping crsql_db_versions inside the file).
//
// The push cursor lives in a plain (non-CRR) `hive_sync_meta` table INSIDE
// the replica file, so it commits with the same durability as the data it
// describes and never replicates.

import {
  encodeCrrSyncRequest, splitCrrSyncReply,
  encodeCrrSyncReply, splitCrrSyncRequest,
  toHex, fromHex, BROWSER_MAX_CRR_FRAME,
} from './hcb1.js';

const STATUS = ['ok', 'sync-gap', 'quota-exceeded', 'value-too-large', 'read-only', 'batch-refused'];
// Push chunks keep a wide margin under the frame cap (the envelope adds the
// watermark advertisement + counts).
const PUSH_BUDGET = BROWSER_MAX_CRR_FRAME - 256 * 1024;
const MAX_ROUNDS = 64;

// A minimal request/response wrapper over the worker's envelope protocol.
export class WorkerClient {
  constructor(worker) {
    this.worker = worker;
    this.nextId = 1;
    this.pending = new Map();
    worker.onmessage = (ev) => {
      const msg = ev.data;
      if (!msg || msg.proto !== 'hive-sqlite-worker/1') return;
      const entry = this.pending.get(msg.id);
      if (!entry) return;
      this.pending.delete(msg.id);
      if (msg.op === 'sql-error' || msg.op === 'open-error') entry.reject(new Error(`${msg.name}: ${msg.message}`));
      else entry.resolve(msg);
    };
  }

  call(op, fields = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      // A hung worker op must fail loudly — a silent never-reply wedges the
      // whole session loop with no signal.
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) reject(new Error(`worker op '${op}' timed out after 30s`));
      }, 30000);
      this.pending.set(id, {
        resolve: (v) => { clearTimeout(timer); resolve(v); },
        reject: (e) => { clearTimeout(timer); reject(e); },
      });
      this.worker.postMessage({ proto: 'hive-sqlite-worker/1', id, op, ...fields });
    });
  }
}

export class BrowserDbSync {
  /**
   * @param worker   WorkerClient over the sqlite DedicatedWorker
   * @param node     the hive_browser BrowserNode (wasm)
   * @param capability  the admission capability's server-derived `db` block
   */
  constructor(worker, node, capability) {
    this.worker = worker;
    this.node = node;
    this.cap = capability;
    // The FLEET's acknowledged watermarks per site — the durable push cursor,
    // hydrated from hive_sync_meta and persisted after every reply.
    this.ack = {};
    // Per-DIRECT-peer push cursors (bn-browser-peer-webrtc-mesh), in memory
    // only, keyed by endpoint id.
    //
    // This separation is load-bearing, not tidiness. `this.ack` is ONE cursor
    // shared by every peer it is used for, and `sync-export` uses it as the
    // push selector — so letting a browser peer advance it would tell this
    // replica "the fleet already has up to version N" on the word of a peer
    // that is NOT the system of record. A browser that had received those rows
    // from a third browser (never from the fleet) would silently strip them
    // from every future fleet push, stranding local writes outside the fleet
    // replica set entirely. Direct peers therefore get their own cursors,
    // which resets on reload cost nothing but a replayed push (the responder
    // reports `replay` and applies nothing), while the fleet cursor keeps
    // exactly its pre-change durable-resume semantics.
    this.directAck = new Map();
  }

  dbFile() { return this.cap.db_file; }

  async open() {
    const opened = await this.worker.call('open', { db: this.dbFile() });
    // Schema is server-derived (the admission capability carries the spec
    // verbatim); cr-sqlite v0.17 does not replicate it, so both halves
    // derive it from the same source.
    for (const table of this.cap.schema ?? []) {
      await this.worker.call('exec', { sql: table.ddl });
      await this.worker.call('as-crr', { table: table.name });
    }
    await this.worker.call('exec', {
      sql: 'CREATE TABLE IF NOT EXISTS hive_sync_meta (k TEXT PRIMARY KEY NOT NULL, v TEXT);',
    });
    const rows = await this.worker.call('exec', { sql: "SELECT k, v FROM hive_sync_meta WHERE k LIKE 'ack:%'" });
    for (const [k, v] of rows.rows ?? []) this.ack[k.slice(4)] = Number(v);
    return opened;
  }

  async saveAck(siteHex, version) {
    this.ack[siteHex] = version;
    await this.worker.call('exec', {
      sql: `INSERT INTO hive_sync_meta (k, v) VALUES ('ack:${siteHex}', '${version}')` +
        ` ON CONFLICT(k) DO UPDATE SET v=excluded.v`,
    });
  }

  async state() { return this.worker.call('sync-state'); }

  async readRows(sql) {
    const r = await this.worker.call('exec', { sql });
    return r.rows;
  }

  async write(sql, ts) {
    await this.worker.call('set-ts', { ts: String(ts ?? Date.now()) });
    return this.worker.call('exec', { sql });
  }

  // One full sync session against ONE peer: multi-round until the push
  // stream is exhausted AND the peer has nothing more to export. Returns a
  // per-session report (the witness harness asserts on these fields).
  //
  // TRANSPORT IS INJECTABLE (bn-browser-peer-webrtc-mesh) and nothing else
  // about a round changes with it: a peer entry may carry its own
  // `send(requestBytes) => Promise<Uint8Array>` — the direct browser↔browser
  // WebRTC DataChannel — and the round semantics (watermarks, chunking, the
  // multi-round termination rule) are identical, because per-site watermarks
  // make the exchange indifferent to WHICH replica answered. Absent, the
  // transport is the node's iroh lane exactly as before, so a peer set that
  // never gained a direct link behaves bit-for-bit like the pre-change client.
  // The ONE thing that does differ is which push cursor a reply may advance —
  // see `directAck` in the constructor.
  async syncPeer(peer) {
    const direct = peer.direct === true;
    const report = {
      peer: peer.endpoint_id, direct, rounds: 0, pushedBatches: 0,
      appliedBatches: 0, replayedBatches: 0, statuses: [], messages: [], forbidden: false,
    };
    const send = typeof peer.send === 'function'
      ? peer.send
      : (bytes) => this.node.crrSyncOn(peer.addr_json, bytes);
    let ack = this.ack;
    if (direct) {
      if (!this.directAck.has(peer.endpoint_id)) this.directAck.set(peer.endpoint_id, {});
      ack = this.directAck.get(peer.endpoint_id);
    }
    for (let round = 0; round < MAX_ROUNDS; round++) {
      report.rounds++;
      const state = await this.state();
      const watermarks = state.watermarks.map((w) => [fromHex(w.siteId), BigInt(w.version)]);
      // Push everything the peer has not yet acknowledged, chunked so each
      // request frame fits the wire cap.
      const exported = await this.worker.call('sync-export', {
        since: ack, maxValueBytes: this.cap.max_value_bytes,
      });
      if (exported.skippedOversized > 0) {
        report.messages.push(
          `export skipped ${exported.skippedOversized} oversized value(s) (first: ${JSON.stringify(exported.firstOversized)})`);
      }
      const chunks = [];
      let cur = [];
      let budget = PUSH_BUDGET;
      for (const hexBatch of exported.batches) {
        const bytes = fromHex(hexBatch);
        if (bytes.length > budget && cur.length) { chunks.push(cur); cur = []; budget = PUSH_BUDGET; }
        if (bytes.length > PUSH_BUDGET) {
          // One batch alone exceeds the frame: it can never travel — loud,
          // stays local (the fleet answers the same way).
          report.messages.push(`push batch of ${bytes.length} bytes exceeds the wire frame; staying local`);
          continue;
        }
        budget -= bytes.length;
        cur.push(bytes);
      }
      if (cur.length) chunks.push(cur);

      // Every round sends at least one request, even with an empty push:
      // that request is also the PULL (the watermarks it advertises are the
      // peer's export selector).
      const sequence = chunks.length ? chunks : [[]];
      let reply = null;
      for (let i = 0; i < sequence.length; i++) {
        const chunk = sequence[i];
        const pushMore = i < sequence.length - 1;
        const request = encodeCrrSyncRequest({ dbFile: this.cap.db_file, pushMore, watermarks, batches: chunk });
        let replyBytes;
        try {
          replyBytes = await send(request);
        } catch (err) {
          // A reset carries no status byte; any refusal here ends the
          // session. Mid-session refusal IS the revocation/expiry signal
          // (contract §5 — the caller decides sealed-vs-wipe via a
          // re-admission attempt); record it verbatim, never swallowed.
          // A DIRECT peer's refusal is NOT that signal: a DataChannel that
          // died says nothing about the fleet's grant, so the caller must not
          // seal on it (see the `direct` flag on the report).
          report.forbidden = true;
          report.messages.push(`sync request refused: ${String(err?.message ?? err)}`);
          return report;
        }
        reply = splitCrrSyncReply(replyBytes);
        const statusName = STATUS[reply.status] ?? `unknown(${reply.status})`;
        report.statuses.push(statusName);
        if (reply.message) report.messages.push(reply.message);
        report.pushedBatches += chunk.length;
        // The reply's watermarks are the apply acknowledgement — persist
        // them BEFORE applying the export half, so a crash between the two
        // never re-pushes already-acked batches (replay would make it a
        // no-op anyway, but the cursor is the honest progress record).
        for (const [site, version] of reply.watermarks) {
          const hex = toHex(site);
          const v = Number(version);
          if ((ack[hex] ?? 0) >= v) continue;
          // A direct peer's acknowledgement moves ONLY that peer's own cursor:
          // it is not the system of record and must never be able to convince
          // this replica that the fleet has seen a row.
          if (direct) ack[hex] = v;
          else await this.saveAck(hex, v);
        }
        if (reply.batches.length) {
          const applied = await this.worker.call('sync-apply', {
            batches: reply.batches.map(toHex),
            maxValueBytes: this.cap.max_value_bytes,
            maxBytes: this.cap.max_bytes,
          });
          for (const outcome of applied.outcomes) {
            if (outcome.status === 'applied') report.appliedBatches++;
            else if (outcome.status === 'replay') report.replayedBatches++;
            else report.messages.push(`apply outcome: ${JSON.stringify(outcome)}`);
          }
        }
      }
      const pulled = (reply?.batches?.length ?? 0) > 0;
      const pushed = chunks.length > 0;
      // A durable typed refusal (quota/value/batch) won't heal within this
      // session — the content cannot fit or cannot travel; re-pushing it for
      // another 63 rounds is pure waste. Loud stop, cursor untouched (the
      // acks already persisted cover only what actually applied).
      if (reply && [2, 3, 5].includes(reply.status)) return report;
      if (!(reply?.more ?? false) && !pulled && !pushed) return report;
    }
    report.messages.push(`round cap ${MAX_ROUNDS} reached`);
    return report;
  }

  // `peers` defaults to the capability's server-derived fleet set. The caller
  // may pass a wider list (fleet peers PLUS direct browser peers) — the fleet
  // half stays first so convergence never depends on a direct link existing,
  // and a direct peer's refusal does not abort the pass the way a fleet
  // refusal (the revocation signal) does.
  async syncAll(peers) {
    const reports = [];
    for (const peer of peers ?? this.cap.sync_peers ?? []) {
      const report = await this.syncPeer(peer);
      reports.push(report);
      if (report.forbidden && !report.direct) break;
    }
    return reports;
  }
}

export const STATUS_NAMES = STATUS;

// ---- inbound half: serve fleet-initiated sync rounds ----------------------
// The SAME round semantics, responder side: decode the request, apply its
// push batches, export what the requester is missing, encode the reply. The
// wasm (setCrrSyncHandler) calls this with the raw request frame.
export function makeCrrSyncResponder(worker, capability) {
  return async (requestBytes, _signal) => {
    const request = splitCrrSyncRequest(requestBytes);
    if (request.dbFile !== capability.db_file)
      throw new Error(`db_file mismatch: request is for ${request.dbFile}, this worker holds ${capability.db_file}`);
    const watermarks = request.watermarks.map(([site, v]) => [toHex(site), Number(v)]);
    let status = 0;
    let message = '';
    if (request.batches.length) {
      if (capability.access === 'read_only') {
        status = 4; // read-only: apply refused, export still served
        message = 'grant is read-only; push batches refused';
      } else {
        const applied = await worker.call('sync-apply', {
          batches: request.batches.map(toHex),
          maxValueBytes: capability.max_value_bytes,
          maxBytes: capability.max_bytes,
        });
        for (const outcome of applied.outcomes) {
          if (outcome.status === 'applied' || outcome.status === 'replay') continue;
          status = { gap: 1, 'quota-exceeded': 2, 'value-too-large': 3 }[outcome.status] ?? 5;
          message = JSON.stringify(outcome);
          break;
        }
      }
    }
    const since = {};
    for (const [siteHex, v] of watermarks) since[siteHex] = v;
    const exported = await worker.call('sync-export', {
      since, maxValueBytes: capability.max_value_bytes,
    });
    if (exported.skippedOversized > 0) {
      message = `${message}${message ? '; ' : ''}skipped ${exported.skippedOversized} oversized value(s) (first: ${JSON.stringify(exported.firstOversized)})`;
    }
    // Budget the reply under the frame cap; 'more' tells the fleet to
    // re-request (its freshly applied watermarks are the resume cursor).
    const state = await worker.call('sync-state');
    const replyWm = state.watermarks.map((w) => [fromHex(w.siteId), BigInt(w.version)]);
    const batches = [];
    let more = false;
    let budget = BROWSER_MAX_CRR_FRAME - 256 * 1024;
    for (const hexBatch of exported.batches) {
      const bytes = fromHex(hexBatch);
      if (bytes.length > budget) { more = true; break; }
      budget -= bytes.length;
      batches.push(bytes);
    }
    return encodeCrrSyncReply({ status, more, message, watermarks: replyWm, batches });
  };
}
