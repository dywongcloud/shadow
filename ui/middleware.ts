import { clerkMiddleware, createRouteMatcher } from "@clerk/nextjs/server";
import { NextResponse } from "next/server";

// Public routes (no auth required): sign-in/up + the node API proxy + static.
const isPublic = createRouteMatcher([
  "/sign-in(.*)",
  "/sign-up(.*)",
  "/cloud(.*)", // dashboard <-> node admin API proxy
  "/api(.*)",
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
  matcher: ["/((?!_next|.*\\..*).*)", "/"],
};
