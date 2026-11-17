import type { Metadata } from "next";
import { darkViewport, pageMeta } from "@/lib/page-meta";
import { VERCEL } from "@/lib/compare-data";
import { ComparePage } from "@/components/compare/compare-page";

const c = VERCEL;

export const metadata: Metadata = pageMeta({
  title: c.title,
  socialTitle: c.socialTitle,
  description: c.description,
  twitterDescription: c.twitterDescription,
  path: `/compare/vercel`,
});
export const viewport = darkViewport;

export default function Page() {
  return <ComparePage c={c} />;
}
