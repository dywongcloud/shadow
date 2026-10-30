"use client";

import { useEffect } from "react";
import { SignUp } from "@clerk/nextjs";
import { AuthShell } from "@/components/auth-shell";
import { markPendingSignIn } from "@/lib/auth-settle";

export default function Page() {
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
