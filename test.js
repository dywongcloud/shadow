// Integration witness — real fleet, real HTTP, no mocks. Mints a real JWT via
// the server-only x-hive-internal token mint path, then exercises the
// sandbox create/read/list/delete surface across multiple physically
// distinct nodes, proving three things the codebase used to get wrong:
//   1. a sandbox created on the control-plane owner is readable from a node
//      that has never locally seen the project row (the require() UNTAGGED_
//      TENANT fallback + gossip.rs cross-node proxy dispatch)
//   2. an unauthenticated caller never sees another tenant's data even
//      through the same proxy path (resolve_tenant's ANON_TENANT fail-closed)
//   3. sandbox provisioning actually boots a real Firecracker microVM
//      (firecracker_safe_id — underscores in sandbox ids used to crash
//      firecracker's own main() on every attempt, 100% deterministic)
//
// Run: node test.js  (needs SSH access to the fleet via ~/Documents/billing.pem;
// exercises the live api directly on each node's loopback admin port over SSH).

const { execFileSync } = require("node:child_process");
const os = require("node:os");

const KEY = `${os.homedir()}/Documents/billing.pem`;
const OWNER = "170.106.158.151"; // fc-sanjose: current control-plane owner
const NON_OWNER_KNOWN = "170.106.40.67"; // fc-virginia-2: has the ipdr project row locally
const NON_OWNER_UNKNOWN = "43.152.247.70"; // fc-bangkok: does NOT have the ipdr project row locally
const PROJECT = "ipdr"; // real project, team "personal", used throughout this session's live testing
const INTERNAL_TOKEN = "d715cc061c1092f8e40f7059eaea7906d63dd2a795ca1d4b2f59fcadb454a4f6";
const OWNER_EMAIL = "dylanwong007@gmail.com";

function sshCurl(host, args) {
  const out = execFileSync(
    "ssh",
    ["-i", KEY, "-o", "StrictHostKeyChecking=no", `root@${host}`, `curl -s -w '\\nHTTP_%{http_code}' ${args}`],
    { encoding: "utf8", timeout: 45_000 },
  );
  const idx = out.lastIndexOf("\nHTTP_");
  const body = out.slice(0, idx);
  const status = Number(out.slice(idx + 6).trim());
  let parsed = null;
  if (body.trim()) {
    try {
      parsed = JSON.parse(body);
    } catch {
      parsed = { raw: body.trim() };
    }
  }
  return { status, body: parsed };
}

function assert(cond, msg) {
  if (!cond) throw new Error(`FAIL: ${msg}`);
  console.log(`ok: ${msg}`);
}

async function main() {
  // Mint a real owner-scoped JWT via the server-only internal-token path —
  // the same mechanism the dashboard's own backend uses, never a hand-rolled
  // fake claim.
  const mint = sshCurl(
    OWNER,
    `-X POST http://127.0.0.1:8786/v1/token -H 'x-hive-internal: ${INTERNAL_TOKEN}' -H 'Content-Type: application/json' -d '{"sub":"${OWNER_EMAIL}","tenant":"personal","role":"owner","email":"${OWNER_EMAIL}"}'`,
  );
  assert(mint.status === 200 && mint.body.token, "minted a real owner JWT from the live fleet");
  const tok = mint.body.token;
  const auth = `-H "Authorization: Bearer ${tok}"`;

  // 1a. A node that has NEVER locally seen the project row takes the new
  // optimistic-then-owner-reverified path (require()'s UNTAGGED_TENANT
  // fallback) — an anonymous caller still ends up scoped to ANON_TENANT
  // (resolve_tenant's fail-closed default under JWT enforcement), so this
  // must come back empty, never another tenant's data.
  const anonUnknown = sshCurl(NON_OWNER_UNKNOWN, `'http://127.0.0.1:8786/v1/projects/${PROJECT}/sandboxes'`);
  assert(anonUnknown.status === 200 && Array.isArray(anonUnknown.body.sandboxes) && anonUnknown.body.sandboxes.length === 0, "unauthenticated GET from a project-row-unknown node returns empty, not another tenant's sandboxes");

  // 1b. A node that DOES have the project row locally takes the original,
  // unmodified strict path (require_project) — an anonymous caller must be
  // rejected outright, not silently scoped to empty.
  const anonKnown = sshCurl(NON_OWNER_KNOWN, `'http://127.0.0.1:8786/v1/projects/${PROJECT}/sandboxes'`);
  assert(anonKnown.status === 403, "unauthenticated GET from a project-row-known node is rejected outright (strict path unchanged)");

  // 2. Create a real sandbox on the owner — this actually boots a Firecracker
  // microVM (firecracker_safe_id fix); a regression here means every
  // sandbox creation on the platform is broken again.
  const create = sshCurl(
    OWNER,
    `-X POST 'http://127.0.0.1:8786/v1/projects/${PROJECT}/sandboxes' ${auth} -H 'Content-Type: application/json' -d '{"runtime":"node22","name":"gm-test-js-witness"}'`,
  );
  assert(create.status === 200, "create_sandbox returned 200");
  assert(create.body.status === "running", `sandbox actually provisioned (status=${create.body.status}, note=${create.body.note})`);
  const sbxId = create.body.id;

  // 3. Read it back from a node that HAS the project row locally.
  const readKnown = sshCurl(NON_OWNER_KNOWN, `'http://127.0.0.1:8786/v1/projects/${PROJECT}/sandboxes/${sbxId}' ${auth}`);
  assert(readKnown.status === 200 && readKnown.body.id === sbxId, "cross-node read succeeds from a node with the project row locally");

  // 4. Read it back from a node that does NOT have the project row locally —
  // this is the exact case that used to 403 with "project belongs to a
  // different team" before the require() UNTAGGED_TENANT fallback.
  const readUnknown = sshCurl(NON_OWNER_UNKNOWN, `'http://127.0.0.1:8786/v1/projects/${PROJECT}/sandboxes/${sbxId}' ${auth}`);
  assert(readUnknown.status === 200 && readUnknown.body.id === sbxId, "cross-node read succeeds from a node WITHOUT the project row locally");

  // 5. list_sandboxes cross-node from the same project-row-unknown node.
  const listUnknown = sshCurl(NON_OWNER_UNKNOWN, `'http://127.0.0.1:8786/v1/projects/${PROJECT}/sandboxes' ${auth}`);
  assert(listUnknown.status === 200 && listUnknown.body.sandboxes.some((s) => s.id === sbxId), "cross-node list includes the sandbox from a project-row-unknown node");

  // 6. A genuinely nonexistent sandbox id still 404s cleanly through the same
  // proxy path (no hang, no leak, no crash).
  const notFound = sshCurl(OWNER, `'http://127.0.0.1:8786/v1/projects/${PROJECT}/sandboxes/sbx_doesnotexist00000' ${auth}`);
  assert(notFound.status === 404 && notFound.body.code === "SANDBOX_NOT_FOUND", "nonexistent sandbox id 404s cleanly, not swallowed or hung");

  // Cleanup — never leave a live microVM running on shared production
  // capacity after a test.
  const del = sshCurl(OWNER, `-X DELETE 'http://127.0.0.1:8786/v1/projects/${PROJECT}/sandboxes/${sbxId}' ${auth}`);
  assert(del.status === 200, "test sandbox deleted, no leaked microVM");

  console.log("\nAll live-fleet integration checks passed.");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
