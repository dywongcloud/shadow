import Link from "next/link";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";
import { Check } from "lucide-react";

type Tier = {
  name: string;
  price: string;
  period?: string;
  tagline: string;
  features: string[];
  cta: { label: string; href: string };
  variant: "outline" | "primary";
  featured?: boolean;
};

const TIERS: Tier[] = [
  {
    name: "Hobby",
    price: "$0",
    period: "/ forever",
    tagline: "For personal projects and experiments.",
    features: [
      "1 concurrent build",
      "Community support",
      "100GB bandwidth",
      "Preview deployments",
      "Automatic HTTPS",
    ],
    cta: { label: "Start free", href: "/sign-up" },
    variant: "outline",
  },
  {
    name: "Pro",
    price: "$20",
    period: "/month",
    tagline: "For growing teams shipping to production.",
    features: [
      "Everything in Hobby",
      "Unlimited concurrent builds",
      "Custom domains & DNS",
      "Team members & roles",
      "Build cache",
      "Cron & workflows",
      "Email support",
    ],
    cta: { label: "Start Pro trial", href: "/sign-up" },
    variant: "primary",
    featured: true,
  },
  {
    name: "Enterprise",
    price: "Custom",
    tagline: "Self-host across your own global mesh.",
    features: [
      "Everything in Pro",
      "Self-hosted nodes",
      "Data residency & leases",
      "SSO/SAML",
      "Audit logs",
      "Dedicated support & SLA",
    ],
    cta: { label: "Contact sales", href: "/contact" },
    variant: "outline",
  },
];

const FAQS: { q: string; a: string }[] = [
  {
    q: "Can I self-host?",
    a: "Yes. The Enterprise tier lets you run shadw across your own global mesh of self-hosted nodes, with full control over data residency and leases.",
  },
  {
    q: "What counts as a build?",
    a: "A build is a single execution of your project pipeline that produces a deployable artifact. Preview and production deployments each trigger a build.",
  },
  {
    q: "Do you offer a free tier?",
    a: "The Hobby plan is free forever and includes everything you need for personal projects and experiments, including preview deployments and automatic HTTPS.",
  },
  {
    q: "Can I bring my own domain?",
    a: "Absolutely. Custom domains and DNS are included on the Pro plan and above, with automatic HTTPS provisioned for every domain you connect.",
  },
];

const PRIMARY_BTN =
  "inline-flex w-full items-center justify-center gap-2 rounded-full bg-emerald-400 px-6 py-3 text-sm font-semibold text-black transition-transform hover:scale-[1.02]";
const OUTLINE_BTN =
  "inline-flex w-full items-center justify-center gap-2 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10";

export default function PricingPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Pricing"
        title={
          <>
            Simple,{" "}
            <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">
              scalable
            </span>{" "}
            pricing
          </>
        }
        subtitle="Start free. Scale on your own mesh. No surprises."
      />

      <MarketingSection className="pb-20">
        <div className="grid gap-6 lg:grid-cols-3">
          {TIERS.map((tier) => (
            <div
              key={tier.name}
              className={`relative flex flex-col rounded-2xl border border-white/10 bg-white/[0.03] p-7 ${
                tier.featured ? "ring-2 ring-emerald-400/60" : ""
              }`}
            >
              {tier.featured && (
                <div className="absolute -top-3 left-1/2 -translate-x-1/2">
                  <span className="rounded-full bg-emerald-400 px-3 py-1 text-xs font-semibold text-black">
                    Most popular
                  </span>
                </div>
              )}

              <div className="text-lg font-semibold text-white">{tier.name}</div>

              <div className="mt-4 flex items-baseline gap-1">
                <span className="text-4xl font-bold text-white">{tier.price}</span>
                {tier.period && <span className="text-sm text-zinc-500">{tier.period}</span>}
              </div>

              <p className="mt-3 text-sm text-zinc-400">{tier.tagline}</p>

              <ul className="mt-6 space-y-3">
                {tier.features.map((feature) => (
                  <li key={feature} className="flex items-start gap-3">
                    <Check className="h-4 w-4 text-emerald-400 shrink-0" />
                    <span className="text-sm text-zinc-300">{feature}</span>
                  </li>
                ))}
              </ul>

              <div className="mt-8 pt-4">
                <Link
                  href={tier.cta.href}
                  className={tier.variant === "primary" ? PRIMARY_BTN : OUTLINE_BTN}
                >
                  {tier.cta.label}
                </Link>
              </div>
            </div>
          ))}
        </div>
      </MarketingSection>

      <MarketingSection className="py-16">
        <h2 className="text-center text-2xl font-bold text-white sm:text-3xl">
          Frequently asked questions
        </h2>
        <div className="mx-auto mt-10 max-w-3xl space-y-6">
          {FAQS.map((faq) => (
            <div key={faq.q} className="rounded-2xl border border-white/10 bg-white/[0.03] p-7">
              <div className="font-bold text-white">{faq.q}</div>
              <p className="mt-2 text-zinc-400">{faq.a}</p>
            </div>
          ))}
        </div>
      </MarketingSection>
    </MarketingShell>
  );
}
