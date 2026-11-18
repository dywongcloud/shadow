import type { Metadata } from "next";
import { darkViewport, pageMeta } from "@/lib/page-meta";
import { MarketingShell, MarketingHero, MarketingSection } from "@/components/marketing-shell";
import { TipsPanel } from "@/components/landing/tips";
import "@/components/landing/landing.css";

export const metadata: Metadata = pageMeta({
  title: "Tips",
  socialTitle: "Support shadw with a tip",
  description:
    "Send a tip to shadw on Ethereum, Optimism, Base, Arbitrum, Polygon, Avalanche, Monad or ZKsync. One EVM address, listed once, with a copy button.",
  twitterDescription:
    "One address for tips on Ethereum, Optimism, Base, Arbitrum, Polygon, Avalanche, Monad and ZKsync.",
  path: "/tips",
});
export const viewport = darkViewport;

export default function TipsPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Support"
        title="Leave a tip."
        subtitle="If shadw saves you time, you can support it on any of the networks below. One address, the same on every network."
      />
      <MarketingSection className="pb-24 pt-10 sm:pb-28">
        <h2 className="sr-only">Networks and address</h2>
        <div className="mx-auto max-w-[820px]">
          <TipsPanel />
        </div>
      </MarketingSection>
    </MarketingShell>
  );
}
