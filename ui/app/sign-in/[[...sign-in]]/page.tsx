"use client";

import { useEffect } from "react";
import { SignIn } from "@clerk/nextjs";
import { AuthShell } from "@/components/auth-shell";
import { markPendingSignIn } from "@/lib/auth-settle";

export default function Page() {
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
