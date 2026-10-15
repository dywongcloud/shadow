import { clerkFrontendOrigin } from "./lib/clerk-origin.mjs";

/** @type {import('next').NextConfig} */
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const ADMIN_OPS = process.env.HIVE_ADMIN_OPS || ADMIN;

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

const CLERK_CSP_ORIGINS = (() => {
  const derived = clerkFrontendOrigin(process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY);
  const live = (process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY || "").startsWith("pk_live_");
  if (live && derived) return derived;
  return [...new Set([derived, "https://*.clerk.accounts.dev"].filter(Boolean))].join(" ");
})();

const nextConfig = {
  poweredByHeader: false,
  compress: true,
  reactStrictMode: true,
  productionBrowserSourceMaps: false,

  turbopack: {
    root: import.meta.dirname,
  },

  cacheComponents: true,
  partialPrefetching: true,

  serverExternalPackages: ["express", "@workflow/web"],

  images: {
    formats: ["image/avif", "image/webp"],
    deviceSizes: [360, 640, 750, 828, 1080, 1200, 1920, 2048, 2880],
    imageSizes: [16, 24, 32, 48, 64, 96, 128, 256, 384],
    remotePatterns: [
      { protocol: "https", hostname: "avatars.githubusercontent.com" },
      { protocol: "https", hostname: "**.githubusercontent.com" },
    ],
  },

  async rewrites() {
    return {
      beforeFiles: [
        { source: "/wfc", destination: "/wf-console/wfc" },
        { source: "/wfc/:path*", destination: "/wf-console/wfc/:path*" },
        { source: "/assets/:path*", destination: "/wf-console/assets/:path*" },
        { source: "/api/rpc", destination: "/wf-console/wfc/api/rpc" },
        { source: "/api/stream/:path*", destination: "/wf-console/wfc/api/stream/:path*" },
      ],
      afterFiles: [
        { source: "/cloud/v1/zkauth/:path*", destination: "/api/blocked" },
        { source: "/cloud/:path*", destination: `${ADMIN}/:path*` },
        { source: "/ops/:path*", destination: `${ADMIN_OPS}/:path*` },
        ...zoneRewrites(),
      ],
    };
  },

  async redirects() {
    return [
      { source: "/dashboard", destination: "/", permanent: true },
      { source: "/login", destination: "/sign-in", permanent: true },
      { source: "/signup", destination: "/sign-up", permanent: true },
      { source: "/team", destination: "/teams", permanent: true },
      { source: "/blog", destination: "/docs", permanent: true },
      { source: "/case-studies", destination: "/solutions", permanent: true },
      { source: "/marketplace", destination: "/integrations", permanent: false },
      { source: "/workflows/runs/:id", destination: "/workflows/run/:id", permanent: false },
    ];
  },

  async headers() {
    const PUBLIC_CACHE = "public, max-age=300, s-maxage=3600, stale-while-revalidate=86400";
    const PRIVATE_CACHE = "private, max-age=60, stale-while-revalidate=300";
    const NO_STORE = "private, no-store, max-age=0, must-revalidate";
    const LANDING_CACHE = "public, max-age=60, s-maxage=300, stale-while-revalidate=300";
    const STATIC_ASSET_CACHE = "public, max-age=86400, s-maxage=604800, stale-while-revalidate=604800";
    const cc = (value) => [{ key: "Cache-Control", value }];
    const noStorePaths = [
      "/account/:path*",
      "/settings/:path*",
      "/teams/:path*",
      "/network/:path*",
      "/deployments/:path*",
      "/billing/:path*",
      "/admin/:path*",
      "/projects/:project/settings/:path*",
      "/projects/:project",
      "/workflows",
      "/workflows/:path*",
      "/wfc",
      "/wfc/:path*",
    ];
    const publicPaths = [
      "/product/:path*",
      "/solutions/:path*",
      "/features/:path*",
      "/pricing/:path*",
      "/blog/:path*",
      "/case-studies/:path*",
      "/contact/:path*",
      "/privacy/:path*",
      "/docs/:path*",
      "/status/:path*",
    ];
    const devUnsafeEval = process.env.NODE_ENV !== "production" ? " 'unsafe-eval'" : "";
    const CSP_ENFORCED = [
      "default-src 'self'",
      `script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'${devUnsafeEval} ${CLERK_CSP_ORIGINS} https://challenges.cloudflare.com`,
      "style-src 'self' 'unsafe-inline'",
      "img-src 'self' data: blob: https:",
      "font-src 'self' data:",
      `connect-src 'self' ${CLERK_CSP_ORIGINS} https://clerk-telemetry.com https://api.shadw.cloud https://*.shadw.cloud wss://*.shadw.cloud https://*.relay.shadw.app:3343 wss://*.relay.shadw.app:3343`,
      `frame-src 'self' ${CLERK_CSP_ORIGINS} https://challenges.cloudflare.com`,
      "worker-src 'self' blob:",
      "object-src 'none'",
      "base-uri 'self'",
      "form-action 'self'",
      "frame-ancestors 'self'",
      "upgrade-insecure-requests",
    ].join("; ");
    const CSP_REPORT_ONLY = [
      "default-src 'self'",
      `script-src 'self' 'wasm-unsafe-eval'${devUnsafeEval} ${CLERK_CSP_ORIGINS} https://challenges.cloudflare.com`,
      "style-src 'self' 'unsafe-inline'",
      "img-src 'self' data: blob: https:",
      "font-src 'self' data:",
      `connect-src 'self' ${CLERK_CSP_ORIGINS} https://clerk-telemetry.com https://api.shadw.cloud https://*.shadw.cloud wss://*.shadw.cloud https://*.relay.shadw.app:3343 wss://*.relay.shadw.app:3343`,
      `frame-src 'self' ${CLERK_CSP_ORIGINS} https://challenges.cloudflare.com`,
      "worker-src 'self' blob:",
      "object-src 'none'",
      "base-uri 'self'",
      "form-action 'self'",
      "frame-ancestors 'self'",
    ].join("; ");
    return [
      {
        source: "/:path*",
        headers: [
          { key: "X-Content-Type-Options", value: "nosniff" },
          { key: "X-Frame-Options", value: "SAMEORIGIN" },
          { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
          { key: "Permissions-Policy", value: "camera=(), microphone=(), geolocation=(self), payment=(), usb=(), interest-cohort=()" },
          { key: "Content-Security-Policy", value: CSP_ENFORCED },
          { key: "Content-Security-Policy-Report-Only", value: CSP_REPORT_ONLY },
          { key: "Strict-Transport-Security", value: "max-age=63072000; includeSubDomains; preload" },
        ],
      },
      { source: "/_next/static/:path*", headers: cc("public, max-age=31536000, immutable") },
      {
        source: "/:file(globe-wireframe(?:-(?:1024|1536))?\\.(?:webp|png))",
        headers: cc(STATIC_ASSET_CACHE),
      },
      ...noStorePaths.map((source) => ({ source, headers: cc(NO_STORE) })),
      ...publicPaths.map((source) => ({ source, headers: cc(PUBLIC_CACHE) })),
      {
        source:
          "/((?!account|settings|teams|network|deployments|billing|admin|product|solutions|features|pricing|blog|case-studies|contact|privacy|docs|status|cloud|api|_next|sign-in|sign-up|workflows|wfc|assets)[^.]*)",
        headers: cc(PRIVATE_CACHE),
      },
      { source: "/", headers: cc(LANDING_CACHE) },
    ];
  },
};

export default nextConfig;
