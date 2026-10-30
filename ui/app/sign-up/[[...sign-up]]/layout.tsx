import type { Metadata } from "next";
import { pageMeta } from "@/lib/page-meta";

export const metadata: Metadata = {
  ...pageMeta({ title: "Sign up", socialTitle: "Sign up — shadw", description: "Create a shadw account: start free on the Hobby plan.", path: "/sign-up" }),
  robots: { index: false },
};

export default function SignUpLayout({ children }: { children: React.ReactNode }) {
  return children;
}
