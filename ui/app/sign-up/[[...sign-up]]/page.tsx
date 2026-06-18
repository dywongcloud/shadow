"use client";

import { SignUp } from "@clerk/nextjs";
import { AuthShell } from "@/components/auth-shell";

export default function Page() {
  return (
    <AuthShell
      render={(appearance) => <SignUp appearance={appearance} signInUrl="/sign-in" />}
      footer={<>Already have an account? Sign in above.</>}
    />
  );
}
