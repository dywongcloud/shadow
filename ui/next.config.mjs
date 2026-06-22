/** @type {import('next').NextConfig} */
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";

// Multi-Zones: other Next.js apps can own path prefixes and be proxied here, so
// the platform can be split into independently-deployed zones (docs, marketing,
// blog…) behind one origin. Configure with e.g.
//   ZONE_DOCS=https://docs.internal  ZONE_BLOG=https://blog.internal
// Each ZONE_<NAME> maps `/<name>` and `/<name>/:path*` to that zone.
// https://nextjs.org/docs/pages/guides/multi-zones
function zoneRewrites() {
  const out = [];
  for (const [key, dest] of Object.entries(process.env)) {
    if (!key.startsWith("ZONE_") || !dest) continue;
    const name = key.slice("ZONE_".length).toLowerCase();
    const base = String(dest).replace(/\/$/, "");
    out.push({ source: `/${name}`, destination: `${base}/${name}` });
    out.push({ source: `/${name}/:path*`, destination: `${base}/${name}/:path*` });
  }
  return out;
}

const nextConfig = {
  // Production hardening (Next.js production checklist).
  poweredByHeader: false,
  compress: true,
  reactStrictMode: true,
  productionBrowserSourceMaps: false,

  // Enable the instrumentation hook (Next 14). See instrumentation.ts.
  // https://nextjs.org/docs/pages/guides/instrumentation
  experimental: {
    instrumentationHook: true,
  },

  async rewrites() {
    return [
      // SECURITY: the ZK preview endpoints (enroll + proof mint) are how a member
      // gains preview access — they must NOT be reachable from the browser, or a
      // signed-in non-member could self-enroll and mint a proof. They're called
      // only server-to-server by /api/preview-unlock + /api/zk-enroll (which first
      // verify Clerk org membership). Block the public proxy path (matched first).
      { source: "/cloud/v1/zkauth/:path*", destination: "/api/blocked" },
      // Proxy dashboard API calls to a hive-cloud node's admin API (avoids CORS).
      { source: "/cloud/:path*", destination: `${ADMIN}/:path*` },
      // Multi-zone proxies (env-driven).
      ...zoneRewrites(),
    ];
  },

  // Permanent redirects for legacy / convenience paths.
  // https://nextjs.org/docs/pages/guides/redirecting
  async redirects() {
    return [
      { source: "/dashboard", destination: "/", permanent: true },
      { source: "/login", destination: "/sign-in", permanent: true },
      { source: "/signup", destination: "/sign-up", permanent: true },
      { source: "/team", destination: "/teams", permanent: true },
      { source: "/marketplace", destination: "/integrations", permanent: false },
    ];
  },

  // Security headers (production checklist).
  async headers() {
    return [
      {
        source: "/:path*",
        headers: [
          { key: "X-Content-Type-Options", value: "nosniff" },
          { key: "X-Frame-Options", value: "SAMEORIGIN" },
          { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
        ],
      },
    ];
  },
};

export default nextConfig;
