"use client";

import { useUser } from "@clerk/nextjs";

export interface Identity {
  id: string;
  name: string;
  email: string;
  initial: string;
  imageUrl?: string;
}

export const LOCAL_IDENTITY: Identity = { id: "local", name: "You", email: "", initial: "Y" };

function derive(user: ReturnType<typeof useUser>["user"]): Identity {
  const name =
    user?.fullName ||
    user?.firstName ||
    user?.username ||
    user?.primaryEmailAddress?.emailAddress?.split("@")[0] ||
    "You";
  return {
    id: user?.id ?? "local",
    name,
    email: user?.primaryEmailAddress?.emailAddress ?? "",
    initial: (name?.[0] || "Y").toUpperCase(),
    imageUrl: user?.imageUrl,
  };
}

// Calls Clerk's useUser — only rendered when Clerk is enabled (see WithIdentity).
function ClerkIdentity({ children }: { children: (id: Identity) => React.ReactNode }) {
  const { user } = useUser();
  return <>{children(derive(user))}</>;
}

const clerkOn = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

/**
 * Provides the signed-in user's identity (name/email/avatar) to its children via
 * a render prop. Uses Clerk when enabled, otherwise a local default — so the UI
 * reflects the actual logged-in account instead of a hard-coded name.
 */
export function WithIdentity({ children }: { children: (id: Identity) => React.ReactNode }) {
  if (!clerkOn) return <>{children(LOCAL_IDENTITY)}</>;
  return <ClerkIdentity>{children}</ClerkIdentity>;
}
