"use client";

import { useEffect } from "react";
import { SignIn } from "@clerk/nextjs";
import { AuthShell } from "@/components/auth-shell";
import { markPendingSignIn } from "@/lib/auth-settle";

export default function Page() {
  // Marks this as a DELIBERATE sign-in before Clerk's hosted flow takes
  // over, so the post-login redirect back to "/" (a client-side transition
  // that shares this app's root layout — see auth-settle.ts) commits the
  // resulting signed-in view instantly instead of sitting through the
  // anti-flicker debounce, which otherwise made a real first login look
  // like it silently failed.
  useEffect(() => {
    markPendingSignIn();
  }, []);

  return (
    <AuthShell
      render={(appearance) => <SignIn appearance={appearance} signUpUrl="/sign-up" />}
      footer={<>By continuing you agree to the Terms &amp; Privacy Policy.</>}
    />
  );
}
