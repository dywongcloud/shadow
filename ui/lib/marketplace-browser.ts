"use client";

import type { MarketplaceResource } from "@/lib/marketplace";

export class MarketplaceBrowserError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
  ) {
    super(code);
    this.name = "MarketplaceBrowserError";
  }
}

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, { ...init, cache: "no-store" });
  } catch {
    throw new MarketplaceBrowserError(503, "UNAVAILABLE");
  }
  const body = await response.json().catch(() => ({})) as { error?: string };
  if (!response.ok) throw new MarketplaceBrowserError(response.status, body.error || "REQUEST_FAILED");
  return body as T;
}

export function fetchMarketplaceProjectResources(projectId: string): Promise<{ resources: MarketplaceResource[] }> {
  return call(`/api/marketplace/projects/${encodeURIComponent(projectId)}/resources`);
}

export function attachMarketplaceOrderToProject(
  projectId: string,
  orderId: string,
): Promise<{ resource: MarketplaceResource }> {
  return call(`/api/marketplace/projects/${encodeURIComponent(projectId)}/resources`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ marketplace_order_id: orderId }),
  });
}
