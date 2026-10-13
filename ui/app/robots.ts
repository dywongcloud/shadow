import type { MetadataRoute } from "next";

const BASE = (process.env.NEXT_PUBLIC_SITE_URL || "https://shadw.cloud").replace(/\/$/, "");

/** robots.txt (served at /robots.txt): public content is open to all crawlers and explicitly welcomes the major LLM/AI crawlers so docs and marketing can surface in AI search;
 *  the per-user dashboard and API surfaces are disallowed. */
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
