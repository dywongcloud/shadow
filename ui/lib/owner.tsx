"use client";

// Platform-owner gating for the Operations Console (/admin) entry points.
//
// The ENFORCEMENT lives in middleware.ts (ADMIN_EMAILS — keep the two lists in
// sync): any non-owner hitting /admin is bounced server-side. This hook only
// decides whether to SHOW the Ops links in the chrome, so non-owners don't see
// an entry point that would just redirect them.

import { useUser } from "@clerk/nextjs";

const ADMIN_EMAILS = new Set([
  "dylanwong007@gmail.com",
  "dylan@shadw.com",
  "noahbladenbankers@gmail.com",
  "dylan@simplyfi.cloud",
  "dylan@shadw.cloud",
  "dylan@weave.cloud",
]);

const clerkOn = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

/** Inner hook — only ever called when Clerk is enabled (provider mounted). */
function useOwnerViaClerk(): boolean {
  const { user } = useUser();
  return !!user?.emailAddresses?.some((e) => ADMIN_EMAILS.has(e.emailAddress.toLowerCase()));
}

/**
 * Whether the signed-in user is the platform owner (allow-listed email).
 * Local no-auth mode (Clerk off) returns true — there the middleware gate is
 * inactive too, and hiding Ops would only hurt local development.
 */
export function useIsPlatformOwner(): boolean {
  // `clerkOn` is a build-time constant, so this branch is identical on every
  // render — the hook order can never actually change.
  // eslint-disable-next-line react-hooks/rules-of-hooks
  return clerkOn ? useOwnerViaClerk() : true;
}
