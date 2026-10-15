import Link from "next/link";
import type { Metadata } from "next";
import { Mail, MessageSquare, Building2 } from "lucide-react";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";

export const metadata: Metadata = {
  title: "Contact — shadw",
  description:
    "Get in touch with shadw about self-hosting, pricing, or migrating your cloud. Reach sales, support, or enterprise by email.",
  alternates: { canonical: "/contact" },
  openGraph: {
    title: "Contact — shadw",
    description:
      "Get in touch with shadw about self-hosting, pricing, or migrating your cloud. Reach sales, support, or enterprise by email.",
    url: "/contact",
    type: "website",
    siteName: "shadw",
  },
  twitter: {
    card: "summary_large_image",
    title: "Contact — shadw",
    description:
      "Get in touch with shadw about self-hosting, pricing, or migrating your cloud. Reach sales, support, or enterprise by email.",
  },
};


export default function ContactPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Contact"
        title={
          <>
            Let&apos;s <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">talk</span>
          </>
        }
        subtitle="Questions about self-hosting, pricing, or migrating your cloud? We&apos;re here to help."
      />

      <MarketingSection className="grid gap-6 py-16 md:grid-cols-3">
        {[
          { icon: MessageSquare, title: "Sales", desc: "Talk through your use case and self-hosting plan.", cta: "sales@shadw.cloud" },
          { icon: Mail, title: "Support", desc: "Get help with deployments, domains, or the mesh.", cta: "support@shadw.cloud" },
          { icon: Building2, title: "Enterprise", desc: "Region pinning, audit logs and priority support.", cta: "enterprise@shadw.cloud" },
        ].map((c) => (
          <GlowCard key={c.title}>
            <div className="flex h-10 w-10 items-center justify-center rounded-lg border border-white/10 bg-white/5">
              <c.icon className="h-5 w-5 text-emerald-300" />
            </div>
            <div className="mt-4 text-lg font-semibold text-white">{c.title}</div>
            <p className="mt-2 text-sm text-zinc-400">{c.desc}</p>
            <a href={`mailto:${c.cta}`} className="mt-4 inline-block text-sm font-medium text-emerald-300 hover:underline">
              {c.cta}
            </a>
          </GlowCard>
        ))}
      </MarketingSection>

      <MarketingSection className="pb-24">
        <Link href="/" className="text-sm text-zinc-500 hover:text-zinc-300">← Back to home</Link>
      </MarketingSection>
    </MarketingShell>
  );
}
