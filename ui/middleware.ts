import { clerkMiddleware, createRouteMatcher } from "@clerk/nextjs/server";
import { NextResponse } from "next/server";

// Public routes (no auth required): sign-in/up + the node API proxy + static.
const isPublic = createRouteMatcher([
  "/", // public Shadow landing (the dashboard renders here only when signed in)
  "/sign-in(.*)",
  "/sign-up(.*)",
  "/cloud(.*)", // dashboard <-> node admin API proxy
  "/api(.*)",
  "/status(.*)", // public, user-facing incident/status board
]);

// Dev-only escape hatch: set HIVE_AUTH_BYPASS=1 to disable login gating entirely
// (used for headless screenshots / local previews). Never set in production.
const bypass = process.env.HIVE_AUTH_BYPASS === "1";

const clerk = clerkMiddleware((auth, req) => {
  if (!isPublic(req)) {
    auth().protect();
  }
});

// When bypassing, skip Clerk's middleware (and its dev-browser handshake) too.
export default bypass ? () => NextResponse.next() : clerk;

export const config = {
  // Run on app routes. Skip Next internals + static FILES (extension at the end),
  // but still gate app routes that legitimately contain dots (e.g. a domain
  // detail page like /domains/example.com) — otherwise they'd bypass auth.
  matcher: [
    "/((?!_next)(?!.*\\.(?:ico|png|jpg|jpeg|gif|svg|webp|woff2?|ttf|css|js|map|txt|xml|webmanifest)$).*)",
    "/",
  ],
};
