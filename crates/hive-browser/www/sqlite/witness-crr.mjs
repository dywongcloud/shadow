// witness-crr.mjs — CDP driver for the browser↔fleet live CRR exchange
// witness (bn-browser-fleet-crr-exchange). Orchestrated by witness-crr.sh,
// which provides: a running local hive-cloud (HIVE_FORCE_MOCK=1, enforced
// JWT, embedded relay), a real zip deploy per project, a static server for
// crates/hive-browser/www, and this script's arguments.
//
// This script launches REAL Chrome (headless=new — the same engine/OPFS/
// wasm as a headed tab), loads witness-crr.html, and drives the scenarios
// over raw CDP (no automation framework):
//
//   A  bidirectional convergence: browser writes reach the fleet replica;
//      fleet-side writes reach the browser — BOTH through the wire op.
//   B  reload persistence: page reload resumes from the durable watermarks
//      (crsql_db_versions inside the OPFS file + the ack cursor), no full
//      re-sync.
//   C  gap refusal: a hand-crafted push missing its first batch gets a
//      typed sync-gap reply; the fleet holds no partial state; a normal
//      sync then converges.
//   D  caps: an oversized VALUE push gets a typed value-too-large refusal
//      (whole batch refused); a push stream crossing max_bytes gets
//      quota-exceeded — never truncation.
//   E  revocation: DELETE admission -> the next sync request is refused
//      (forbidden); the browser wipes its OPFS replica.
//
// Exit 0 with WITNESS_OK only when every assertion held.

import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';

const args = {};
for (let i = 2; i < process.argv.length; i += 2) args[process.argv[i].replace(/^--/, '')] = process.argv[i + 1];
for (const k of ['api', 'token', 'relay', 'deployment-main', 'deployment-caps', 'db-main', 'db-caps', 'replica-tool', 'page']) {
  if (!args[k]) { console.error(`missing --${k}`); process.exit(2); }
}
// --scenario stale: only the pre-change-binary contrast (the fleet was
// restarted with HIVE_BROWSER_DB_LISTEN=0): a sync round must be REFUSED
// loudly, and the replica file must be byte-state untouched.
const STALE_ONLY = args.scenario === 'stale';

const CHROME = process.env.CHROME_BIN || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const results = [];
let failed = false;
function check(name, cond, detail = '') {
  results.push(`${cond ? 'ok' : 'FAIL'}  ${name}${detail ? ` — ${detail}` : ''}`);
  if (!cond) failed = true;
}

// ---- Chrome + raw CDP plumbing -------------------------------------------
// Chrome launch on a contended machine flakes occasionally (the port file
// never appears); one bounded retry is the honest answer, with stderr on
// failure.
function getJson(url) {
  return new Promise((resolve, reject) => {
    http.get(url, (res) => {
      let body = '';
      res.on('data', (c) => (body += c));
      res.on('end', () => { try { resolve(JSON.parse(body)); } catch (e) { reject(e); } });
    }).on('error', reject);
  });
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(fn, timeoutMs, label) {
  const t0 = Date.now();
  for (;;) {
    try { if (await fn()) return; } catch { /* keep waiting */ }
    if (Date.now() - t0 > timeoutMs) throw new Error(`timeout waiting for ${label}`);
    await sleep(250);
  }
}

let profile, chrome, chromeErr, chromeDead;
async function launchChrome() {
  profile = fs.mkdtempSync('/tmp/crr-witness-chrome-');
  chromeErr = [];
  chromeDead = null;
  chrome = spawn(CHROME, [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
    '--no-first-run', '--no-default-browser-check', '--disable-extensions', 'about:blank',
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  chrome.stderr.on('data', (c) => {
    for (const line of String(c).split('\n')) if (line.trim()) chromeErr.push(line.trim());
    if (chromeErr.length > 200) chromeErr.splice(0, chromeErr.length - 200);
  });
  chrome.on('exit', (code, signal) => { chromeDead = { code, signal }; });
  const portFile = `${profile}/DevToolsActivePort`;
  const t0 = Date.now();
  while (Date.now() - t0 < 25000) {
    if (fs.existsSync(portFile)) return fs.readFileSync(portFile, 'utf8').split('\n')[0].trim();
    if (chromeDead) break;
    await sleep(250);
  }
  const tail = chromeErr.slice(-10).join(' | ');
  try { chrome.kill('SIGKILL'); } catch { /* already dead */ }
  try { fs.rmSync(profile, { recursive: true, force: true, maxRetries: 3, retryDelay: 300 }); } catch { /* leave it */ }
  throw new Error(`chrome launch failed (exit ${JSON.stringify(chromeDead)}): ${tail}`);
}
let cdpPort;
const consoleLines = [];
try {
for (let attempt = 1; attempt <= 2; attempt++) {
  try {
    cdpPort = await launchChrome();
    break;
  } catch (err) {
    if (attempt === 2) throw err;
    console.log(`chrome launch attempt 1 failed (${err.message.slice(0, 200)}); retrying once`);
  }
}
const portFile = `${profile}/DevToolsActivePort`;
const targets = await getJson(`http://127.0.0.1:${cdpPort}/json/list`);
const pageTarget = targets.find((t) => t.type === 'page');
if (!pageTarget) throw new Error('no page target');

const ws = new WebSocket(pageTarget.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
let msgId = 0;
const pending = new Map();
ws.onclose = () => {
  for (const { reject } of pending.values()) {
    reject(new Error(`CDP websocket closed (chrome exit: ${JSON.stringify(chromeDead)}); stderr tail: ${chromeErr.slice(-12).join(' | ')}`));
  }
  pending.clear();
};
ws.onmessage = (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
  }
};
function cdp(method, params = {}) {
  const id = ++msgId;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
}
await cdp('Page.enable');
await cdp('Runtime.enable');
await cdp('Log.enable').catch(() => {});
ws.addEventListener('message', (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.method === 'Runtime.consoleAPICalled') {
    const line = `console.${msg.params.type}: ` + msg.params.args.map((a) => a.value ?? a.description ?? '').join(' ');
    // The wasm's tracing-wasm subscriber is chatty at DEBUG/TRACE; keep
    // signal lines only.
    if (/%c(DEBUG|TRACE)%c/.test(line) && !/error|fail|panic|warn/i.test(line)) return;
    consoleLines.push(line);
  } else if (msg.method === 'Runtime.exceptionThrown') {
    consoleLines.push(`page-exception: ${JSON.stringify(msg.params.exceptionDetails.exception?.description ?? msg.params.exceptionDetails.text)}`);
  }
});

async function evalJs(expression) {
  const r = await cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) {
    throw new Error(`page eval failed: ${JSON.stringify(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text)} :: ${expression.slice(0, 120)}`);
  }
  return r.result?.value;
}
const nav = async (url) => cdp('Page.navigate', { url });

function fleetRows(dbPath) {
  const out = execFileSync(args['replica-tool'], ['rows', dbPath], { encoding: 'utf8' });
  return JSON.parse(out.trim().replace(/^rows: /, ''));
}
function fleetWrite(dbPath, id, label, ts) {
  return execFileSync(args['replica-tool'], ['write', dbPath, String(id), label, String(ts)], { encoding: 'utf8' }).trim();
}
async function postJson(url) {
  const resp = await fetch(url, {
    method: 'POST',
    headers: { authorization: `Bearer ${args.token}`, 'content-type': 'application/json' },
  });
  const body = await resp.json().catch(() => null);
  return { http: resp.status, ...((body && typeof body === 'object') ? body : { body }) };
}

const DEPLOY_MAIN = args['deployment-main'];
const DEPLOY_CAPS = args['deployment-caps'];
const DB_MAIN = args['db-main'];
const DB_CAPS = args['db-caps'];
const pageUrl = (deployment = DEPLOY_MAIN) =>
  `${args.page}?api=${encodeURIComponent(args.api)}&token=${encodeURIComponent(args.token)}` +
  `&relay=${encodeURIComponent(args.relay)}&deployment=${encodeURIComponent(deployment)}&function=api`;

  // ---- boot + admission ---------------------------------------------------
  await nav(pageUrl());
  await waitFor(async () => evalJs('!!window.witness && (window.witness.ready || window.witness.log.some(l => l.step === "boot-failed"))'), 90000, 'page boot');
  const bootLog = await evalJs('window.witness.log');
  const bootFail = bootLog.find((l) => l.step === 'boot-failed');
  check('page boot + admission with db capability', !bootFail, bootFail ? `${bootFail.error} ${bootFail.detail ?? ''} ${(bootFail.stack ?? '').slice(0, 600)}` : 'ready');
  if (bootFail) throw new Error('cannot continue without admission');

  if (STALE_ONLY) {
    // The pre-change-binary contrast (contract §7): the fleet was restarted
    // with HIVE_BROWSER_DB_LISTEN=0, so every sync request is refused
    // loudly (NO_HANDLER — a pre-change binary's UNKNOWN_OP is the same
    // refusal class) and the replica file must be untouched.
    const admitStale = bootLog.find((l) => l.step === 'admit');
    check('stale-node: admission still issued with real sync peers',
      (admitStale?.db?.sync_peers?.length ?? 0) >= 1,
      JSON.stringify(admitStale?.db?.sync_peers ?? []));
    const fleetBefore = fleetRows(DB_MAIN);
    const syncStale = await evalJs(`witness.sync('${DEPLOY_MAIN}')`);
    check('stale-node: sync refused (no grants served)', syncStale.length === 0 || syncStale[0].forbidden === true,
      JSON.stringify(syncStale));
    check('stale-node: refusal reached the peer (not a vacuous no-peer pass)',
      syncStale.length >= 1, JSON.stringify(syncStale));
    const fleetAfter = fleetRows(DB_MAIN);
    check('stale-node: replica untouched by the refused round',
      JSON.stringify(fleetBefore) === JSON.stringify(fleetAfter),
      `before=${JSON.stringify(fleetBefore)} after=${JSON.stringify(fleetAfter)}`);
  } else {
  const admit = bootLog.find((l) => l.step === 'admit');
  check('capability.db is server-derived and complete',
    !!admit?.db?.db_file?.startsWith('hive-browserdb-')
    && admit.db.access === 'read_write'
    && Array.isArray(admit.db.schema) && admit.db.schema.length === 1
    && Array.isArray(admit.db.sync_peers) && admit.db.sync_peers.length >= 1,
    JSON.stringify({ db_file: admit?.db?.db_file, access: admit?.db?.access, peers: admit?.db?.sync_peers?.length, caps: [admit?.db?.max_bytes, admit?.db?.max_value_bytes] }));

  // ---- A: bidirectional convergence ---------------------------------------
  await evalJs(`witness.write('${DEPLOY_MAIN}', 'browser-row-1')`);
  await evalJs(`witness.write('${DEPLOY_MAIN}', 'browser-row-2')`);
  // A0: the FLEET-INITIATED direction — BrowserPool::crr_sync via the
  // operator endpoint, before ANY browser-initiated sync has run.
  const endpointId = await evalJs('witness.endpointId()');
  const pull = await postJson(`${args.api}/v1/browser/dbs/sync/${endpointId}`);
  check('A0: fleet-initiated pull round ok', pull.status === 'ok' && pull.reply_batches >= 1, JSON.stringify(pull));
  const rowsFleetA0 = fleetRows(DB_MAIN);
  check('A0: fleet pulled browser-origin rows with no browser-initiated sync',
    JSON.stringify(rowsFleetA0).includes('browser-row-1') && JSON.stringify(rowsFleetA0).includes('browser-row-2'),
    JSON.stringify(rowsFleetA0));
  const fw = fleetWrite(DB_MAIN, 1, 'fleet-row-1', Date.now());
  const syncA = await evalJs(`witness.sync('${DEPLOY_MAIN}')`);
  const repA = syncA[0];
  check('A: sync round ok', repA.statuses.every((s) => s === 'ok'), JSON.stringify(repA.statuses));
  check('A: browser pushes its batches', repA.pushedBatches >= 1, `pushed=${repA.pushedBatches}`);
  check('A: browser applies fleet batches', repA.appliedBatches >= 1, `applied=${repA.appliedBatches}`);
  const rowsBrowserA = await evalJs(`witness.rows('${DEPLOY_MAIN}')`);
  const rowsFleetA = fleetRows(DB_MAIN).map(([id, label]) => [id, String(label)]);
  const norm = (rows) => JSON.stringify(rows.map(([id, label]) => [Number(id), String(label)]));
  check('A: converged browser-side', norm(rowsBrowserA) === norm(rowsFleetA), `browser=${norm(rowsBrowserA)} fleet=${norm(rowsFleetA)}`);
  check('A: fleet holds the browser-origin rows', norm(rowsFleetA).includes('browser-row-1') && norm(rowsFleetA).includes('browser-row-2'));
  check('A: browser holds the fleet-origin row', norm(rowsBrowserA).includes('fleet-row-1'), fw);

  // ---- B: reload persistence ----------------------------------------------
  const stateBefore = await evalJs(`witness.state('${DEPLOY_MAIN}')`);
  const ackBefore = await evalJs(`witness.ack('${DEPLOY_MAIN}')`);
  await nav(pageUrl());
  await waitFor(async () => evalJs('!!window.witness && (window.witness.ready || window.witness.log.some(l => l.step === "boot-failed"))'), 90000, 'page reload');
  const syncB1 = await evalJs(`witness.sync('${DEPLOY_MAIN}')`);
  const repB1 = syncB1[0];
  check('B: after reload, no full re-push (watermark resume)', repB1.pushedBatches === 0, `pushed=${repB1.pushedBatches}`);
  check('B: after reload, no re-apply needed', repB1.appliedBatches === 0, `applied=${repB1.appliedBatches} replayed=${repB1.replayedBatches}`);
  const rowsBrowserB = await evalJs(`witness.rows('${DEPLOY_MAIN}')`);
  check('B: OPFS replica intact across reload', norm(rowsBrowserB) === norm(rowsFleetA), `rows=${norm(rowsBrowserB)}`);
  const stateAfter = await evalJs(`witness.state('${DEPLOY_MAIN}')`);
  check('B: durable watermarks survive reload', stateAfter.watermarks.length >= stateBefore.watermarks.length,
    `before=${JSON.stringify(stateBefore.watermarks)} ack=${JSON.stringify(ackBefore)} after=${JSON.stringify(stateAfter.watermarks)}`);
  await evalJs(`witness.write('${DEPLOY_MAIN}', 'browser-row-3')`);
  const syncB2 = await evalJs(`witness.sync('${DEPLOY_MAIN}')`);
  check('B: post-reload write syncs incrementally', syncB2[0].pushedBatches === 1, `pushed=${syncB2[0].pushedBatches}`);
  const rowsFleetB = fleetRows(DB_MAIN);
  check('B: fleet converged after incremental push', rowsFleetB.length === 4, `fleet=${JSON.stringify(rowsFleetB)}`);

  // ---- C: gap refusal -------------------------------------------------------
  await evalJs(`witness.write('${DEPLOY_MAIN}', 'browser-row-4')`);
  await evalJs(`witness.write('${DEPLOY_MAIN}', 'browser-row-5')`);
  const gap = await evalJs(`witness.gapProbe('${DEPLOY_MAIN}')`);
  check('C: dropped batch -> typed sync-gap refusal', gap.status === 1, JSON.stringify(gap));
  const rowsFleetC = fleetRows(DB_MAIN);
  check('C: fleet holds no partial state from the refused push', rowsFleetC.length === 4, `fleet=${JSON.stringify(rowsFleetC)}`);
  const syncC = await evalJs(`witness.sync('${DEPLOY_MAIN}')`);
  const rowsFleetC2 = fleetRows(DB_MAIN);
  check('C: normal sync converges after the gap', rowsFleetC2.length === 6 && syncC[0].statuses.every((s) => s === 'ok'),
    `fleet=${JSON.stringify(rowsFleetC2)} statuses=${JSON.stringify(syncC[0].statuses)}`);

  // ---- D: caps (second project, small caps, FRESH PAGE) --------------------
  // One AccessHandlePoolVFS can own /hive-crsql per origin at a time — the
  // single-writer design. The caps project therefore runs on a fresh page
  // load (the old page's worker dies with it), which also re-admits the
  // endpoint to the caps deployment (renewal replaces the crrmain grant).
  await nav(pageUrl(DEPLOY_CAPS));
  await waitFor(async () => evalJs('!!window.witness && (window.witness.ready || window.witness.log.some(l => l.step === "boot-failed"))'), 90000, 'caps page boot');
  const capsBoot = await evalJs('window.witness.log');
  const capsFail = capsBoot.find((l) => l.step === 'boot-failed');
  check('D: caps page boot + admission', !capsFail, capsFail ? `${capsFail.error} ${capsFail.detail ?? ''}` : 'ready');
  if (capsFail) throw new Error('cannot continue without the caps admission');
  const admitCaps = capsBoot.find((l) => l.step === 'admit');
  check('D: caps project admitted with small resolved caps',
    admitCaps.db.max_bytes === 262144 && admitCaps.db.max_value_bytes === 65536,
    JSON.stringify({ max_bytes: admitCaps.db.max_bytes, max_value_bytes: admitCaps.db.max_value_bytes }));
  // value cap, glue-side (export filter): an oversized LOCAL value never
  // leaves the browser and is named loudly.
  await evalJs(`witness.write('${DEPLOY_CAPS}', 'small-1')`);
  await evalJs(`witness.writeSized('${DEPLOY_CAPS}', 100000)`);
  const syncD1 = await evalJs(`witness.sync('${DEPLOY_CAPS}')`);
  const msgsD1 = syncD1[0].messages.join(' | ');
  check('D: oversized local value refused at export (stays local, named)', /skipped 1 oversized/.test(msgsD1), msgsD1);
  const rowsCapsFleet1 = fleetRows(DB_CAPS);
  check('D: fleet holds only the within-cap row', rowsCapsFleet1.length === 1 && String(rowsCapsFleet1[0][1]) === 'small-1',
    JSON.stringify(rowsCapsFleet1));
  const rowsCapsBrowser1 = await evalJs(`witness.rows('${DEPLOY_CAPS}')`);
  check('D: browser keeps its oversized row (never truncated)', rowsCapsBrowser1.length === 2, JSON.stringify(rowsCapsBrowser1));
  // value cap, FLEET-side typed refusal + whole-batch rollback (hand-crafted
  // push, since the glue's own filter would skip it first).
  const over = await evalJs(`witness.oversizedProbe('${DEPLOY_CAPS}', 100000)`);
  check('D: fleet refuses oversized push typed value-too-large', over.status === 3, JSON.stringify(over));
  check('D: fleet state unchanged by the refused batch', fleetRows(DB_CAPS).length === 1);
  // quota: a push stream crossing max_bytes gets quota-exceeded, whole-batch.
  await evalJs(`witness.writeSized('${DEPLOY_CAPS}', 60000)`);
  await evalJs(`witness.writeSized('${DEPLOY_CAPS}', 60000)`);
  await evalJs(`witness.writeSized('${DEPLOY_CAPS}', 60000)`);
  await evalJs(`witness.writeSized('${DEPLOY_CAPS}', 60000)`);
  const syncD2 = await evalJs(`witness.sync('${DEPLOY_CAPS}')`);
  check('D: quota-exceeded surfaced typed', syncD2[0].statuses.includes('quota-exceeded'), JSON.stringify(syncD2[0].statuses));
  const capsFileBytes = fs.statSync(DB_CAPS).size;
  check('D: replica never grew past cap + one refused batch (no truncation)',
    capsFileBytes < 262144 + 128 * 1024, `file=${capsFileBytes} bytes`);
  const rowsCapsFleet2 = fleetRows(DB_CAPS).length;
  const rowsCapsBrowser2 = (await evalJs(`witness.rows('${DEPLOY_CAPS}')`)).length;
  check('D: quota refusal is not truncation (fleet subset, browser whole)',
    rowsCapsFleet2 < rowsCapsBrowser2, `fleet=${rowsCapsFleet2} browser=${rowsCapsBrowser2}`);

  // ---- D2: stale-grant cross-contamination guard (wire level) --------------
  // The endpoint's LIVE grant is now crrcaps. A request naming crrmain's
  // db_file must be refused BEFORE any decode/apply — and the caps replica
  // must stay byte-state untouched.
  const capsFleetBefore = fleetRows(DB_CAPS);
  const wrong = await evalJs(`witness.wrongDbProbe('${DEPLOY_CAPS}', 'hive-browserdb-crrmain.db')`);
  check('D2: wrong-db_file request refused (stale grant cannot sync)', wrong.refused === true, JSON.stringify(wrong));
  check('D2: caps replica untouched by the refused request',
    JSON.stringify(fleetRows(DB_CAPS)) === JSON.stringify(capsFleetBefore));

  // ---- E: revocation (fresh page back on crrmain) ---------------------------
  await nav(pageUrl(DEPLOY_MAIN));
  await waitFor(async () => evalJs('!!window.witness && (window.witness.ready || window.witness.log.some(l => l.step === "boot-failed"))'), 90000, 'revocation page boot');
  const eBoot = await evalJs('window.witness.log');
  const eFail = eBoot.find((l) => l.step === 'boot-failed');
  check('E: crrmain page re-boot + re-admission', !eFail, eFail ? `${eFail.error} ${eFail.detail ?? ''}` : 'ready');
  if (eFail) throw new Error('cannot run revocation without the admission');
  const revoke = await evalJs('witness.revoke()');
  check('E: admission revoked', revoke.status === 200, JSON.stringify(revoke));
  const syncE = await evalJs(`witness.sync('${DEPLOY_MAIN}')`);
  check('E: post-revoke sync refused (replication cut)', syncE.length === 0 || syncE[0].forbidden === true,
    JSON.stringify(syncE));
  await evalJs(`witness.wipe('${DEPLOY_MAIN}')`);
  const reopen = await evalJs(`witness.reopenState('${DEPLOY_MAIN}')`);
  check('E: OPFS replica wiped (fresh file knows no sites)', reopen.watermarks.length === 0,
    JSON.stringify(reopen));
  }
} catch (err) {
  failed = true;
  results.push(`FAIL  driver exception — ${err.message}`);
} finally {
  // Grab the page log BEFORE killing Chrome (the dump needs a live target),
  // then cleanup ALWAYS runs — a crashed driver must not orphan a headless
  // Chrome on an already contended machine.
  try { const dump = await evalJs('window.witness.log'); results.push(`page-log: ${JSON.stringify(dump).slice(0, 4000)}`); } catch { /* page may be mid-navigation */ }
  try {
    if (chrome) chrome.kill('SIGKILL');
    await Promise.race([chrome ? new Promise((res) => chrome.once('exit', res)) : Promise.resolve(), sleep(3000)]);
  } catch { /* already dead */ }
  try { if (profile) fs.rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 500 }); } catch { /* leave the tmpdir */ }
}

for (const line of results) console.log(line);
if (consoleLines.length) console.log(`console: ${consoleLines.slice(0, 40).join(' || ').slice(0, 4000)}`);
console.log(failed ? 'WITNESS_FAIL' : 'WITNESS_OK');
process.exit(failed ? 1 : 0);
