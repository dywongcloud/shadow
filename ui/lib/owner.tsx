"use client";

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

/** Whether the signed-in user is the platform owner (allow-listed email): display gating only, `ui/proxy.ts` enforces `/admin` with its own copy of the list (keep both in sync).
 *  Local no-auth mode (Clerk off) returns true, since the proxy gate is inactive there too and hiding Ops would only hurt local development. */
export function useIsPlatformOwner(): boolean {
  // eslint-disable-next-line react-hooks/rules-of-hooks
  return clerkOn ? useOwnerViaClerk() : true;
}
