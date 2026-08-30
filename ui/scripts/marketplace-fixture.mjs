#!/usr/bin/env node
/**
 * Local-only Marketplace placement-policy fixture.
 *
 * This intentionally has no token minting or auth bypass: every request is
 * verified against the configured Clerk issuer's JWKS and required audience.
 * Select a manual scenario with the order id: valid, unauthorized, missing,
 * tenant-mismatch, expired, revoked, suspended, malformed, unsupported, or
 * no-eligible-nodes (for example, `fixture-expired`).
 */
import http from "node:http";
import { createRemoteJWKSet, jwtVerify } from "jose";

const port = Number(process.env.MARKETPLACE_FIXTURE_PORT || 4010);
const issuer = required("MARKETPLACE_FIXTURE_CLERK_ISSUER").replace(/\/$/, "");
const audience = required("MARKETPLACE_FIXTURE_CLERK_AUDIENCE");
const approvedNode = process.env.MARKETPLACE_FIXTURE_APPROVED_NODE_ID || "node-a";
const jwks = createRemoteJWKSet(new URL(`${issuer}/.well-known/jwks.json`));

function required(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required; this fixture never accepts unsigned or unverified JWTs.`);
  return value;
}

function json(response, status, body) {
  response.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  response.end(JSON.stringify(body));
}

async function authenticate(request) {
  const header = request.headers.authorization;
  if (!header?.startsWith("Bearer ")) throw new Error("missing bearer token");
  const { payload } = await jwtVerify(header.slice(7), jwks, { issuer, audience });
  if (typeof payload.sub !== "string" || !payload.sub) throw new Error("token has no subject");
  return payload;
}

function tenant(payload) {
  return typeof payload.org_id === "string" && payload.org_id
    ? `clerk:org:${payload.org_id}`
    : `clerk:user:${payload.sub}`;
}

function placementPolicy(orderId, buyerTenantId, scenario) {
  const now = Date.now();
  const policy = {
    contract_version: scenario === "unsupported" ? 2 : 1,
    policy_version: 1,
    marketplace_order_id: orderId,
    buyer_tenant_id: scenario === "tenant-mismatch" ? "clerk:user:someone-else" : buyerTenantId,
    status: scenario === "suspended" ? "suspended" : "active",
    revocation_state: scenario === "revoked" ? "revoked" : "not_revoked",
    valid_from: new Date(now - 60_000).toISOString(),
    valid_until: new Date(scenario === "expired" ? now - 1 : now + 60 * 60_000).toISOString(),
    approved_node_ids: scenario === "no-eligible-nodes" ? ["fixture-unavailable-node"] : [approvedNode],
    provider_id: "fixture-provider",
    listing_id: "fixture-listing",
    region: "local",
    resources: { vcpu: 1, memory_mb: 512, disk_gb: 1 },
    commercial: { currency: "USD", price_cents: 0 },
  };
  if (scenario === "malformed") policy.unexpected_private_metadata = "rejected";
  return policy;
}

http.createServer(async (request, response) => {
  const match = request.url?.match(/^\/v1\/marketplace\/orders\/([^/]+)\/placement-policy$/);
  if (request.method !== "GET" || !match) return json(response, 404, { error: "not found" });
  try {
    const claims = await authenticate(request);
    const orderId = decodeURIComponent(match[1]);
    const scenario = orderId.replace(/^fixture-/, "");
    if (scenario === "unauthorized") return json(response, 403, { error: "unauthorized" });
    if (scenario === "missing") return json(response, 404, { error: "policy missing" });
    return json(response, 200, placementPolicy(orderId, tenant(claims), scenario));
  } catch {
    return json(response, 401, { error: "unauthorized" });
  }
}).listen(port, "127.0.0.1", () => {
  console.log(`Marketplace fixture listening on http://127.0.0.1:${port}`);
});
