"use client";

import dynamic from "next/dynamic";
import { useSettledAuth } from "@/lib/auth-settle";
import { TopNav } from "@/components/topnav";
import { Footer } from "@/components/footer";
import { ZkPreviewAuth } from "@/components/zk-preview-auth";
import { PendingBuildsProvider } from "@/components/pending-builds-provider";
import { SessionToken } from "@/components/session-token";

const GitOps = dynamic(() => import("@/components/gitops").then((m) => m.GitOps), {
  ssr: false,
  loading: () => null,
});
const CommandBar = dynamic(() => import("@/components/command-bar").then((m) => m.CommandBar), {
  ssr: false,
  loading: () => null,
});

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

export function ChromeTop() {
  if (!clerkEnabled) return <TopNav />;
  return <GuardedChromeTop />;
}

function GuardedChromeTop() {
  const view = useSettledAuth();
  if (view !== "in") return null;
  return <TopNav />;
}

export function ChromeBottom() {
  if (!clerkEnabled) {
    return (
      <>
        <Footer />
        <GitOps />
        <CommandBar />
        <PendingBuildsProvider />
        {process.env.NEXT_PUBLIC_HIVE_DEV_MINT === "1" && <SessionToken />}
      </>
    );
  }
  return <GuardedChromeBottom />;
}

function GuardedChromeBottom() {
  const view = useSettledAuth();
  if (view !== "in") return null;
  return (
    <>
      <Footer />
      <GitOps />
      <CommandBar />
      <ZkPreviewAuth />
      <PendingBuildsProvider />
      <SessionToken />
    </>
  );
}
