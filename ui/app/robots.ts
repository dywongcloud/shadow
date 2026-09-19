import type { MetadataRoute } from "next";

const BASE = (process.env.NEXT_PUBLIC_SITE_URL || "https://autheo.dev").replace(/\/$/, "");

/**
 * robots.txt (served at /robots.txt). Public content is open to all crawlers,
 * and — per the AI-search era — explicitly welcomes the major LLM/AI crawlers so
 * autheo's docs + marketing can surface in ChatGPT / Perplexity / Claude / Google
 * AI Overviews. Private, per-user dashboard + API surfaces are disallowed.
 */
export default function robots(): MetadataRoute.Robots {
  const privatePaths = [
    "/api/", "/cloud/", "/ops/", "/admin", "/account", "/settings",
    "/billing", "/teams", "/network", "/deployments", "/projects",
    "/sign-in", "/sign-up",
  ];
  const aiCrawlers = [
    "GPTBot", "OAI-SearchBot", "ChatGPT-User", "PerplexityBot", "Perplexity-User",
    "ClaudeBot", "Claude-Web", "anthropic-ai", "Google-Extended", "Applebot-Extended", "CCBot",
  ];
  return {
    rules: [
      { userAgent: "*", allow: "/", disallow: privatePaths },
      { userAgent: aiCrawlers, allow: "/", disallow: privatePaths },
    ],
    sitemap: `${BASE}/sitemap.xml`,
    host: BASE,
  };
}
