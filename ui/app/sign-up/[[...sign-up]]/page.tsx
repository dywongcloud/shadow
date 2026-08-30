"use client";

import { useEffect } from "react";
import { SignUp } from "@clerk/nextjs";
import { AuthShell } from "@/components/auth-shell";
import { markPendingSignIn } from "@/lib/auth-settle";

export default function Page() {
  // Same fix as the sign-in page: sign-up also completes into an
  // authenticated session and hits the identical stale-`committed`,
  // client-side-redirect problem — see auth-settle.ts's `markPendingSignIn`.
  useEffect(() => {
    markPendingSignIn();
  }, []);

  return (
    <AuthShell
      render={(appearance) => <SignUp appearance={appearance} signInUrl="/sign-in" />}
      footer={<>Already have an account? Sign in above.</>}
    />
  );
}
