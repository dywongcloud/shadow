import type { MetadataRoute } from "next";

const BASE = (process.env.NEXT_PUBLIC_SITE_URL || "https://shadw.cloud").replace(/\/$/, "");

/**
 * sitemap.xml (served at /sitemap.xml) — the public, crawlable surface for search
 * + AI discovery. Only universal-content routes are listed (never per-user
 * dashboard pages).
 */
export default function sitemap(): MetadataRoute.Sitemap {
  const now = new Date();
  const routes: { path: string; freq: MetadataRoute.Sitemap[number]["changeFrequency"]; priority: number }[] = [
    { path: "", freq: "daily", priority: 1 },
    { path: "/product", freq: "weekly", priority: 0.9 },
    { path: "/solutions", freq: "weekly", priority: 0.8 },
    { path: "/features", freq: "weekly", priority: 0.8 },
    { path: "/pricing", freq: "weekly", priority: 0.9 },
    { path: "/docs", freq: "weekly", priority: 0.9 },
    { path: "/docs/getting-started", freq: "weekly", priority: 0.8 },
    { path: "/docs/api-reference", freq: "weekly", priority: 0.7 },
    { path: "/blog", freq: "weekly", priority: 0.6 },
    { path: "/case-studies", freq: "weekly", priority: 0.6 },
    { path: "/contact", freq: "monthly", priority: 0.5 },
    { path: "/privacy", freq: "yearly", priority: 0.3 },
  ];
  return routes.map((r) => ({
    url: `${BASE}${r.path}`,
    lastModified: now,
    changeFrequency: r.freq,
    priority: r.priority,
  }));
}
