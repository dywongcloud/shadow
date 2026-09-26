import type { MetadataRoute } from "next";

// PWA manifest — served at /manifest.webmanifest. Makes Autheo DevHub installable as a
// standalone app on desktop and mobile.
export default function manifest(): MetadataRoute.Manifest {
  return {
    name: "Autheo DevHub",
    short_name: "DevHub",
    description:
      "Autheo DevHub is a self-hosted peer-to-peer cloud: deploy serverless functions, containers, and static sites to your own electric mesh.",
    id: "/",
    start_url: "/",
    scope: "/",
    display: "standalone",
    orientation: "any",
    background_color: "#050b07",
    theme_color: "#0f8a3b",
    categories: ["developer", "productivity", "utilities"],
    icons: [
      { src: "/web-app-manifest-192x192.png", sizes: "192x192", type: "image/png", purpose: "any" },
      { src: "/web-app-manifest-512x512.png", sizes: "512x512", type: "image/png", purpose: "any" },
      { src: "/web-app-manifest-192x192.png", sizes: "192x192", type: "image/png", purpose: "maskable" },
      { src: "/web-app-manifest-512x512.png", sizes: "512x512", type: "image/png", purpose: "maskable" },
    ],
    // bn-ui-pwa-install-offline: "Run a node" as a distinguished alternate
    // entry point (its own launch target from the installed app's icon —
    // desktop right-click, Android long-press), NOT a second manifest/service
    // worker/cache-versioning stack of its own. The existing sw.js already
    // covers install/offline/caching for every route under this one scope
    // (including /run-node, which is same-origin and same-scope); duplicating
    // that machinery per-route would risk drifting out of sync with its
    // carefully-tuned network-first invariants for no real benefit here.
    shortcuts: [
      {
        name: "Run a node",
        short_name: "Run a node",
        description: "Donate spare browser capacity to serve one of your deployed functions.",
        url: "/run-node",
        icons: [{ src: "/web-app-manifest-192x192.png", sizes: "192x192", type: "image/png" }],
      },
    ],
  };
}
