"use client";

import { SignIn } from "@clerk/nextjs";
import { AuthShell } from "@/components/auth-shell";

export default function Page() {
  return (
    <AuthShell
      render={(appearance) => <SignIn appearance={appearance} signUpUrl="/sign-up" />}
      footer={<>By continuing you agree to the Terms &amp; Privacy Policy.</>}
    />
  );
}
