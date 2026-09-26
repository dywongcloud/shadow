export type MarketplaceResourceType = "compute" | "storage";
export type MarketplaceResource = {
  order_id: string;
  listing_name: string;
  provider_name: string;
  resource_type: MarketplaceResourceType;
  status: string;
  capacity: Record<string, string | number | boolean>;
};

const FALLBACK_MARKETPLACE_URL =
  process.env.NODE_ENV === "production"
    ? "https://marketplace.autheo.dev"
    : "http://localhost:3000";

/** Public Marketplace base URL. This is intentionally a non-secret URL. */
export function marketplaceUrl(): string {
  const configured = process.env.NEXT_PUBLIC_MARKETPLACE_URL?.trim();
  try {
    return new URL(configured || FALLBACK_MARKETPLACE_URL).toString().replace(/\/$/, "");
  } catch {
    return FALLBACK_MARKETPLACE_URL;
  }
}

/**
 * A Marketplace browsing URL that keeps the purchase tied to its originating
 * DevHub project. Marketplace returns to this exact project URL and appends
 * `marketplace_order=<id>` after it creates an order.
 */
export function marketplaceListingsUrl(
  projectId: string,
  resourceType: MarketplaceResourceType,
  returnUrl: string,
): string {
  const url = new URL("/listings", `${marketplaceUrl()}/`);
  url.searchParams.set("project", projectId);
  url.searchParams.set("resource_type", resourceType);
  url.searchParams.set("return_url", returnUrl);
  return url.toString();
}

export function projectMarketplaceReturnUrl(projectId: string, origin: string): string {
  return new URL(`/projects/${encodeURIComponent(projectId)}`, origin).toString();
}
