/** @type {import('next').NextConfig} */
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";

const nextConfig = {
  // Proxy dashboard API calls to a hive-cloud node's admin API (avoids CORS).
  async rewrites() {
    return [{ source: "/cloud/:path*", destination: `${ADMIN}/:path*` }];
  },
};

export default nextConfig;
