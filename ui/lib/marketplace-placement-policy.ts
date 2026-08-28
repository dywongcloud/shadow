import "server-only";

const POLICY_CONTRACT_VERSION = 1;
const NODE_ID = /^[A-Za-z0-9_-]{1,128}$/;
const SENSITIVE_KEY = /(address|credential|secret|token|claim|control.?plane|private|metadata)/i;

export type MarketplacePlacementSnapshot = {
  contract_version: number;
  policy_version: number;
  marketplace_order_id: string;
  buyer_tenant_id: string;
  retrieved_at_ms: number;
  approved_node_ids: string[];
  policy: Record<string, unknown>;
};

export class MarketplacePolicyError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "MarketplacePolicyError";
  }
}

/** The opaque Marketplace buyer identity is derived only from Clerk's verified session. */
export function clerkMarketplaceTenant(session: { userId: string | null; orgId: string | null }): string {
  if (!session.userId) throw new MarketplacePolicyError(401, "NO_SESSION", "A Clerk-authenticated session is required.");
  return session.orgId ? `clerk:org:${session.orgId}` : `clerk:user:${session.userId}`;
}

function object(value: unknown, field: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new MarketplacePolicyError(422, "MALFORMED_POLICY", `${field} must be an object.`);
  }
  return value as Record<string, unknown>;
}

function text(value: unknown, field: string): string {
  if (typeof value !== "string" || !value.trim()) {
    throw new MarketplacePolicyError(422, "MALFORMED_POLICY", `${field} must be a non-empty string.`);
  }
  return value;
}

function positiveSafeInteger(value: unknown, field: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
    throw new MarketplacePolicyError(422, "MALFORMED_POLICY", `${field} must be a positive integer.`);
  }
  return value;
}

function instant(value: unknown, field: string): number {
  const parsed = typeof value === "string" ? Date.parse(value) : Number.NaN;
  if (!Number.isFinite(parsed)) throw new MarketplacePolicyError(422, "MALFORMED_POLICY", `${field} must be an ISO-8601 timestamp.`);
  return parsed;
}

function rejectSensitiveData(value: unknown, path = "policy"): void {
  if (Array.isArray(value)) {
    value.forEach((item, index) => rejectSensitiveData(item, `${path}[${index}]`));
    return;
  }
  if (!value || typeof value !== "object") return;
  for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
    if (SENSITIVE_KEY.test(key)) {
      throw new MarketplacePolicyError(422, "MALFORMED_POLICY", `${path}.${key} is not allowed in a deployment snapshot.`);
    }
    rejectSensitiveData(child, `${path}.${key}`);
  }
}

function validateCommercial(value: unknown): void {
  const commercial = object(value, "commercial");
  const currency = text(commercial.currency, "commercial.currency");
  if (!/^[A-Z]{3}$/.test(currency)) {
    throw new MarketplacePolicyError(422, "MALFORMED_POLICY", "commercial.currency must be an ISO-4217 currency.");
  }
  if (typeof commercial.price_cents !== "number" || !Number.isSafeInteger(commercial.price_cents) || commercial.price_cents < 0) {
    throw new MarketplacePolicyError(422, "MALFORMED_POLICY", "commercial.price_cents must be a non-negative integer.");
  }
}

function validateResources(value: unknown): void {
  const resources = object(value, "resources");
  for (const field of ["vcpu", "memory_mb", "disk_gb"]) {
    if (typeof resources[field] !== "number" || !Number.isFinite(resources[field]) || Number(resources[field]) < 0) {
      throw new MarketplacePolicyError(422, "MALFORMED_POLICY", `resources.${field} must be a non-negative number.`);
    }
  }
}

/**
 * Validate and canonicalize the Marketplace v1 schema. This intentionally
 * accepts no unknown top-level fields so private addresses, credentials,
 * control-plane metadata, and raw Clerk identity material cannot be retained
 * in a deployment record.
 */
export function validateMarketplacePlacementPolicy(
  response: unknown,
  marketplaceOrderId: string,
  buyerTenantId: string,
  now = Date.now(),
): MarketplacePlacementSnapshot {
  const policy = object(response, "policy");
  const allowed = new Set([
    "contract_version", "policy_version", "marketplace_order_id", "buyer_tenant_id",
    "status", "revocation_state", "valid_from", "valid_until", "approved_node_ids",
    "provider_id", "listing_id", "region", "resources", "commercial",
  ]);
  for (const key of Object.keys(policy)) {
    if (!allowed.has(key)) throw new MarketplacePolicyError(422, "MALFORMED_POLICY", `Unexpected policy field: ${key}.`);
  }
  const contractVersion = positiveSafeInteger(policy.contract_version, "contract_version");
  if (contractVersion !== POLICY_CONTRACT_VERSION) {
    throw new MarketplacePolicyError(409, "INCOMPATIBLE_VERSION", "Unsupported Marketplace placement-policy contract version.");
  }
  const policyVersion = positiveSafeInteger(policy.policy_version, "policy_version");
  if (text(policy.marketplace_order_id, "marketplace_order_id") !== marketplaceOrderId) {
    throw new MarketplacePolicyError(403, "ORDER_MISMATCH", "Marketplace policy does not belong to the requested order.");
  }
  if (text(policy.buyer_tenant_id, "buyer_tenant_id") !== buyerTenantId) {
    throw new MarketplacePolicyError(403, "TENANT_MISMATCH", "Marketplace policy buyer tenant does not match this Clerk-authenticated request.");
  }
  if (policy.status !== "active") throw new MarketplacePolicyError(409, "POLICY_INACTIVE", "Marketplace placement policy is not active.");
  if (policy.revocation_state !== "not_revoked") throw new MarketplacePolicyError(409, "POLICY_REVOKED", "Marketplace placement policy is revoked.");
  const validFrom = instant(policy.valid_from, "valid_from");
  const validUntil = instant(policy.valid_until, "valid_until");
  if (validFrom > now || validUntil <= now || validUntil <= validFrom) {
    throw new MarketplacePolicyError(409, "POLICY_EXPIRED", "Marketplace placement policy is outside its validity window.");
  }
  if (!Array.isArray(policy.approved_node_ids) || policy.approved_node_ids.length === 0) {
    throw new MarketplacePolicyError(422, "MALFORMED_POLICY", "approved_node_ids must be a non-empty list.");
  }
  const approvedNodeIds = policy.approved_node_ids.map((id) => text(id, "approved_node_ids[]"));
  if (new Set(approvedNodeIds).size !== approvedNodeIds.length || approvedNodeIds.some((id) => !NODE_ID.test(id))) {
    throw new MarketplacePolicyError(422, "MALFORMED_POLICY", "approved_node_ids contains an invalid or duplicate node id.");
  }
  text(policy.provider_id, "provider_id");
  text(policy.listing_id, "listing_id");
  text(policy.region, "region");
  validateResources(policy.resources);
  validateCommercial(policy.commercial);
  rejectSensitiveData(policy);

  return {
    contract_version: contractVersion,
    policy_version: policyVersion,
    marketplace_order_id: marketplaceOrderId,
    buyer_tenant_id: buyerTenantId,
    retrieved_at_ms: now,
    approved_node_ids: approvedNodeIds,
    policy,
  };
}

const RETRY_DELAYS_MS = [100, 300, 900];

/** Fetches only transient Marketplace failures again; policy/auth failures are terminal. */
export async function fetchMarketplacePlacementPolicy(
  marketplaceUrl: string,
  marketplaceOrderId: string,
  clerkJwt: string,
): Promise<unknown> {
  let url: URL;
  try {
    url = new URL(`/v1/marketplace/orders/${encodeURIComponent(marketplaceOrderId)}/placement-policy`, marketplaceUrl);
  } catch {
    throw new MarketplacePolicyError(500, "MARKETPLACE_CONFIG", "MARKETPLACE_URL is not configured correctly.");
  }
  let lastNetworkError: unknown;
  for (let attempt = 0; attempt <= RETRY_DELAYS_MS.length; attempt += 1) {
    try {
      const response = await fetch(url, {
        headers: { authorization: `Bearer ${clerkJwt}`, accept: "application/json" },
        cache: "no-store",
      });
      if (response.ok) return await response.json();
      if (response.status < 500) {
        throw new MarketplacePolicyError(response.status, `MARKETPLACE_HTTP_${response.status}`, "Marketplace placement policy was not authorized or is unavailable.");
      }
      lastNetworkError = new Error(`Marketplace HTTP ${response.status}`);
    } catch (error) {
      if (error instanceof MarketplacePolicyError) throw error;
      lastNetworkError = error;
    }
    if (attempt < RETRY_DELAYS_MS.length) {
      await new Promise((resolve) => setTimeout(resolve, RETRY_DELAYS_MS[attempt]));
    }
  }
  throw new MarketplacePolicyError(503, "MARKETPLACE_UNAVAILABLE", `Marketplace placement policy is temporarily unavailable: ${String(lastNetworkError)}`);
}
