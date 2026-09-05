import "server-only";

import { marketplaceUrl, type MarketplaceResource, type MarketplaceResourceType } from "@/lib/marketplace";

export type { MarketplaceResource };

export class MarketplaceIntegrationError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "MarketplaceIntegrationError";
  }
}

function asText(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function resourceType(value: unknown): MarketplaceResourceType {
  if (value === "compute" || value === "storage") return value;
  throw new MarketplaceIntegrationError(502, "MALFORMED_RESPONSE", "Marketplace returned an invalid resource type.");
}

function capacity(value: unknown): Record<string, string | number | boolean> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  const entries = Object.entries(value).filter(([, item]) =>
    typeof item === "string" || typeof item === "number" || typeof item === "boolean",
  );
  return Object.fromEntries(entries);
}

function parseResource(value: unknown): MarketplaceResource {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new MarketplaceIntegrationError(502, "MALFORMED_RESPONSE", "Marketplace returned an invalid project resource.");
  }
  const item = value as Record<string, unknown>;
  const orderId = asText(item.order_id || item.marketplace_order_id);
  if (!orderId) throw new MarketplaceIntegrationError(502, "MALFORMED_RESPONSE", "Marketplace resource is missing its order ID.");
  return {
    order_id: orderId,
    listing_name: asText(item.listing_name || item.listing_id, "Marketplace listing"),
    provider_name: asText(item.provider_name || item.provider_id, "Marketplace provider"),
    resource_type: resourceType(item.resource_type),
    status: asText(item.status, "pending"),
    capacity: capacity(item.capacity || item.specification || item.resources),
  };
}

async function request<T>(path: string, clerkJwt: string, init?: RequestInit): Promise<T> {
  const url = new URL(path, `${marketplaceUrl()}/`);
  let response: Response;
  try {
    response = await fetch(url, {
      ...init,
      headers: { accept: "application/json", authorization: `Bearer ${clerkJwt}`, ...init?.headers },
      cache: "no-store",
    });
  } catch {
    throw new MarketplaceIntegrationError(503, "UNAVAILABLE", "Marketplace is unavailable.");
  }
  if (!response.ok) {
    throw new MarketplaceIntegrationError(
      response.status,
      response.status === 401 ? "UNAUTHENTICATED" : response.status === 404 ? "UNAVAILABLE" : "REQUEST_FAILED",
      "Marketplace could not complete the project resource request.",
    );
  }
  return response.json() as Promise<T>;
}

export async function listMarketplaceProjectResources(projectId: string, clerkJwt: string): Promise<MarketplaceResource[]> {
  const response = await request<{ resources?: unknown[] }>(
    `/v1/marketplace/projects/${encodeURIComponent(projectId)}/resources`,
    clerkJwt,
  );
  if (!Array.isArray(response.resources)) {
    throw new MarketplaceIntegrationError(502, "MALFORMED_RESPONSE", "Marketplace returned an invalid resources response.");
  }
  return response.resources.map(parseResource);
}

export async function attachMarketplaceOrder(
  projectId: string,
  orderId: string,
  clerkJwt: string,
): Promise<MarketplaceResource> {
  const response = await request<unknown>(
    `/v1/marketplace/orders/${encodeURIComponent(orderId)}/project-attachments`,
    clerkJwt,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ project_id: projectId }),
    },
  );
  return parseResource(response);
}
