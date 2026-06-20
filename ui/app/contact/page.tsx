import Link from "next/link";
import { Mail, MessageSquare, Building2 } from "lucide-react";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";

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
          { icon: Building2, title: "Enterprise", desc: "Data residency, SSO, audit logs and SLAs.", cta: "enterprise@shadw.cloud" },
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
        <GlowCard className="mx-auto max-w-2xl">
          <h2 className="text-2xl font-bold tracking-tight text-white">Send us a message</h2>
          <p className="mt-2 text-sm text-zinc-400">We typically reply within one business day.</p>
          <div className="mt-6 grid gap-4 sm:grid-cols-2">
            <input placeholder="Name" className="rounded-lg border border-white/15 bg-white/5 px-4 py-2.5 text-sm text-white outline-none placeholder:text-zinc-500 focus:border-white/30" />
            <input placeholder="Work email" className="rounded-lg border border-white/15 bg-white/5 px-4 py-2.5 text-sm text-white outline-none placeholder:text-zinc-500 focus:border-white/30" />
          </div>
          <textarea placeholder="How can we help?" rows={4} className="mt-4 w-full rounded-lg border border-white/15 bg-white/5 px-4 py-2.5 text-sm text-white outline-none placeholder:text-zinc-500 focus:border-white/30" />
          <div className="mt-5 flex items-center justify-between">
            <Link href="/" className="text-sm text-zinc-500 hover:text-zinc-300">← Back to home</Link>
            <button className="inline-flex items-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black transition-transform hover:scale-[1.03]">
              Send message
            </button>
          </div>
        </GlowCard>
      </MarketingSection>
    </MarketingShell>
  );
}
