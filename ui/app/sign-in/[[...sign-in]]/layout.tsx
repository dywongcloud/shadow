import type { Metadata } from "next";
import { pageMeta } from "@/lib/page-meta";

export const metadata: Metadata = {
  ...pageMeta({ title: "Sign in", socialTitle: "Sign in — shadw", description: "Sign in to shadw.", path: "/sign-in" }),
  robots: { index: false },
};

export default function SignInLayout({ children }: { children: React.ReactNode }) {
  return children;
}
