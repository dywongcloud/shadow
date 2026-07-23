/** @type {import('next').NextConfig} */
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
// Ops/admin console API base. The developer/API-key surface is `/cloud` -> ADMIN
// (api.shadw.cloud); the operator console (/admin pages) proxies through `/ops`
// -> ADMIN_OPS (admin.shadw.cloud) so ops traffic uses the dedicated host.
// Defaults to ADMIN so nothing breaks before the admin host is live.
const ADMIN_OPS = process.env.HIVE_ADMIN_OPS || ADMIN;

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

  // The upstream @workflow/web console (mounted at /workflows via
  // app/wf-console/[[...slug]]/route.ts) loads its compiled Express app from
  // node_modules at runtime — keep it (and express) external so Next doesn't
  // inline the dashboard build and its dynamic asset paths keep resolving.
  serverExternalPackages: ["express", "@workflow/web"],

  // Image Optimization (Vercel-parity): next/image serves responsive, correctly
  // sized, modern-format images with lazy loading by default. AVIF/WebP first;
  // the default device/image size ladder covers phones→desktops. remotePatterns
  // allowlists the known external hosts the UI renders (GitHub avatars); truly
  // arbitrary provider-logo URLs stay a plain lazy <img> (see the components).
  images: {
    formats: ["image/avif", "image/webp"],
    deviceSizes: [360, 640, 750, 828, 1080, 1200, 1920, 2048],
    imageSizes: [16, 24, 32, 48, 64, 96, 128, 256, 384],
    remotePatterns: [
      { protocol: "https", hostname: "avatars.githubusercontent.com" },
      { protocol: "https", hostname: "**.githubusercontent.com" },
    ],
  },

  // instrumentation.ts is a stable convention in Next 16 — the experimental
  // `instrumentationHook` flag was removed (it now errors as an unknown key).

  async rewrites() {
    return {
      // Upstream @workflow/web workflow console (isolated mount). These MUST
      // run beforeFiles: hive ships a native app/workflows page, and
      // filesystem routes would otherwise shadow the mount. The native page
      // stays untouched on disk — these rewrites simply own the URL. The
      // compiled React Router app is re-based to basename /workflows by
      // scripts/patch-wf-console.mjs; every /workflows/* URL (HTML, .data
      // loader fetches, /workflows/api/rpc CBOR, /workflows/api/stream/*,
      // /workflows/__manifest) plus its content-hashed /assets/* files is
      // forwarded into the wf-console bridge, which strips the /wf-console
      // prefix and dispatches to the upstream Express app.
      beforeFiles: [
        { source: "/workflows", destination: "/wf-console/workflows" },
        { source: "/workflows/:path*", destination: "/wf-console/workflows/:path*" },
        { source: "/assets/:path*", destination: "/wf-console/assets/:path*" },
        // Belt-and-suspenders for the root-absolute API URLs the upstream
        // client would use if an unpatched bundle ever ships: hive has no
        // /api/rpc or /api/stream routes, so these can't collide.
        { source: "/api/rpc", destination: "/wf-console/workflows/api/rpc" },
        { source: "/api/stream/:path*", destination: "/wf-console/workflows/api/stream/:path*" },
      ],
      afterFiles: [
        // SECURITY: the ZK preview endpoints (enroll + proof mint) are how a member
        // gains preview access — they must NOT be reachable from the browser, or a
        // signed-in non-member could self-enroll and mint a proof. They're called
        // only server-to-server by /api/preview-unlock + /api/zk-enroll (which first
        // verify Clerk org membership). Block the public proxy path (matched first).
        { source: "/cloud/v1/zkauth/:path*", destination: "/api/blocked" },
        // Proxy dashboard API calls to a hive-cloud node's admin API (avoids CORS).
        { source: "/cloud/:path*", destination: `${ADMIN}/:path*` },
        // Ops console → the dedicated admin host (admin.shadw.cloud).
        { source: "/ops/:path*", destination: `${ADMIN_OPS}/:path*` },
        // Multi-zone proxies (env-driven).
        ...zoneRewrites(),
      ],
    };
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
      // hive's legacy run-detail URL shape (/workflows/runs/<id>) → the
      // upstream console's route (/workflows/run/<id>) so old links land on
      // the mounted @workflow/web run view.
      { source: "/workflows/runs/:id", destination: "/workflows/run/:id", permanent: false },
    ];
  },

  // Security headers (production checklist) + client/CDN caching to cut traffic.
  async headers() {
    // Reasonable cache windows. Public marketing/docs are shared-cacheable; the
    // hashed Next build assets are immutable. Sensitive surfaces (personal/team/
    // project settings, project deployments, network, billing, admin) are never
    // cached. The /cloud API proxy and /api routes are left untouched (live
    // per-request data). The middleware applies the same policy as a backstop.
    const PUBLIC_CACHE = "public, max-age=300, s-maxage=3600, stale-while-revalidate=86400";
    const PRIVATE_CACHE = "private, max-age=60, stale-while-revalidate=300";
    const NO_STORE = "private, no-store, max-age=0, must-revalidate";
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
    ];
    const publicPaths = [
      "/",
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
    // CSP is now ENFORCING (promoted from report-only). The policy allows the
    // app's real origins (verified from the codebase: client data goes
    // same-origin through /cloud + /api/* server proxies → `connect-src 'self'`;
    // Clerk auth origins; GitHub avatar images via `img-src https:`). `script-src`
    // carries `'unsafe-inline'` because Next.js emits inline hydration/bootstrap
    // scripts and this build has no per-request nonce pipeline — the OTHER
    // directives still deliver the high-value protections (connect-src bounds
    // exfiltration destinations; object-src/base-uri/form-action/frame-ancestors
    // close clickjacking, base-href hijack and cross-origin form posting).
    const CSP_ENFORCED = [
      "default-src 'self'",
      "script-src 'self' 'unsafe-inline' https://*.clerk.accounts.dev https://challenges.cloudflare.com",
      "style-src 'self' 'unsafe-inline'",
      "img-src 'self' data: blob: https:",
      "font-src 'self' data:",
      "connect-src 'self' https://*.clerk.accounts.dev https://clerk-telemetry.com https://api.shadw.cloud https://*.shadw.cloud",
      "frame-src 'self' https://*.clerk.accounts.dev https://challenges.cloudflare.com",
      "worker-src 'self' blob:",
      "object-src 'none'",
      "base-uri 'self'",
      "form-action 'self'",
      "frame-ancestors 'self'",
      "upgrade-insecure-requests",
    ].join("; ");
    // Report-Only carries the STRICTER next target: the same policy WITHOUT
    // `script-src 'unsafe-inline'`. Violations reported here map exactly the
    // inline scripts a future per-request-nonce middleware must cover before
    // `'unsafe-inline'` can be dropped from the enforcing policy above.
    const CSP_REPORT_ONLY = [
      "default-src 'self'",
      "script-src 'self' https://*.clerk.accounts.dev https://challenges.cloudflare.com",
      "style-src 'self' 'unsafe-inline'",
      "img-src 'self' data: blob: https:",
      "font-src 'self' data:",
      "connect-src 'self' https://*.clerk.accounts.dev https://clerk-telemetry.com https://api.shadw.cloud https://*.shadw.cloud",
      "frame-src 'self' https://*.clerk.accounts.dev https://challenges.cloudflare.com",
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
          { key: "Permissions-Policy", value: "camera=(), microphone=(), geolocation=(), payment=(), usb=(), interest-cohort=()" },
          { key: "Content-Security-Policy", value: CSP_ENFORCED },
          { key: "Content-Security-Policy-Report-Only", value: CSP_REPORT_ONLY },
          // Only meaningful over HTTPS; harmless (browsers ignore it) when
          // served over plain HTTP in local dev.
          { key: "Strict-Transport-Security", value: "max-age=63072000; includeSubDomains; preload" },
        ],
      },
      // Immutable hashed build assets — cache for a year.
      { source: "/_next/static/:path*", headers: cc("public, max-age=31536000, immutable") },
      // Sensitive / dynamic management surfaces — never cache.
      ...noStorePaths.map((source) => ({ source, headers: cc(NO_STORE) })),
      // Public marketing / docs / status — shared (CDN) + browser cacheable.
      ...publicPaths.map((source) => ({ source, headers: cc(PUBLIC_CACHE) })),
      // Other authenticated dashboard tabs — short private browser cache of the
      // page shell (live data still streams from /cloud). Negative lookahead
      // excludes the sensitive + public + API/asset paths handled above.
      {
        source:
          "/((?!account|settings|teams|network|deployments|billing|admin|product|solutions|features|pricing|blog|case-studies|contact|privacy|docs|status|cloud|api|_next|sign-in|sign-up)[^.]*)",
        headers: cc(PRIVATE_CACHE),
      },
    ];
  },
};

export default nextConfig;
