import "server-only";

import { MarketplacePolicyError } from "@/lib/marketplace-placement-policy";

/**
 * Server-only Marketplace origin for deployment policy retrieval.
 *
 * Keeping this separate from NEXT_PUBLIC_MARKETPLACE_URL prevents the browser
 * configuration used for Marketplace navigation from deciding where privileged
 * Clerk-template JWTs are sent.
 */
export function marketplaceDeploymentUrl(): string {
  const configured = process.env.MARKETPLACE_URL?.trim();
  if (!configured) {
    throw new MarketplacePolicyError(
      503,
      "MARKETPLACE_CONFIG",
      "MARKETPLACE_URL is not configured for server-side Marketplace policy retrieval.",
    );
  }
  try {
    const url = new URL(configured);
    if (url.protocol !== "https:" && url.protocol !== "http:") throw new Error("unsupported protocol");
    return url.toString().replace(/\/$/, "");
  } catch {
    throw new MarketplacePolicyError(
      503,
      "MARKETPLACE_CONFIG",
      "MARKETPLACE_URL must be an absolute HTTP(S) URL.",
    );
  }
}

/** Whether the server-only deployment origin is present and a valid HTTP(S) URL. */
export function marketplaceDeploymentConfigured(): boolean {
  try {
    marketplaceDeploymentUrl();
    return true;
  } catch {
    return false;
  }
}
