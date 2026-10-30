"use client";

import Link from "next/link";
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
      footer={<>By continuing you agree to the <Link href="/privacy" className="underline underline-offset-2 hover:text-fg">Privacy Policy</Link>.</>}
    />
  );
}
