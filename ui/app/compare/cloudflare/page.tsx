import type { Metadata } from "next";
import { darkViewport, pageMeta } from "@/lib/page-meta";
import { CLOUDFLARE } from "@/lib/compare-data";
import { ComparePage } from "@/components/compare/compare-page";

const c = CLOUDFLARE;

export const metadata: Metadata = pageMeta({
  title: c.title,
  socialTitle: c.socialTitle,
  description: c.description,
  twitterDescription: c.twitterDescription,
  path: `/compare/cloudflare`,
});
export const viewport = darkViewport;

export default function Page() {
  return <ComparePage c={c} />;
}
