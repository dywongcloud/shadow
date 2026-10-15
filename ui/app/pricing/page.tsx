import Link from "next/link";
import type { Metadata } from "next";
import { darkViewport, pageMeta } from "@/lib/page-meta";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";
import { Check } from "lucide-react";

export const metadata: Metadata = pageMeta({
  title: "Pricing",
  socialTitle: "Pricing — shadw",
  description:
    "shadw pricing plans: start free on Hobby, ship to production on Pro at $20/month, or choose Enterprise at $1,000/month. Compare tiers and read answers to common pricing questions.",
  twitterDescription:
    "shadw pricing: start free on Hobby, ship to production on Pro at $20/month, or choose Enterprise at $1,000/month. Compare tiers and read pricing FAQs.",
  path: "/pricing",
});
export const viewport = darkViewport;


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

/** Every bullet is a line the plan catalog or a plan gate backs (`crates/hive-cloud/src/billing.rs`: `PLANS`, `plan_allows_*`, `plan_max_*`; the
 *  Enterprise suite is gated in `enterprise_api.rs`): a feature every plan has (preview deployments, HTTPS, domains) is listed once, under Hobby. */
const TIERS: Tier[] = [
  {
    name: "Hobby",
    price: "$0",
    period: "/ forever",
    tagline: "For personal projects and experiments.",
    features: [
      "1 concurrent build",
      "Community support",
      "$5 of included compute / month",
      "Preview deployments",
      "Automatic HTTPS",
      "Custom domains & DNS",
      "Build cache",
      "Cron & workflows",
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
      "$20 of included compute / month",
      "Pay-as-you-go beyond included compute",
      "Up to 25 team members",
      "Deployment protection and microfrontends",
      "30 days of log history",
      "Email support",
    ],
    cta: { label: "Start Pro", href: "/sign-up" },
    variant: "primary",
    featured: true,
  },
  {
    name: "Enterprise",
    price: "$1,000",
    period: "/month",
    tagline: "For organizations with large compute needs.",
    features: [
      "Everything in Pro",
      "$1,000 of included compute / month",
      "Multi-region fail-over",
      "SAML SSO, SCIM and SIEM audit-log streaming",
      "IP blocking",
      "Functions that run up to an hour",
      "Priority support",
    ],
    cta: { label: "Start Enterprise", href: "/sign-up" },
    variant: "outline",
  },
];

const FAQS: { q: string; a: string }[] = [
  {
    q: "Can I self-host?",
    a: "Yes. shadw can run on nodes you operate. Self-hosting is not a plan feature: Hobby, Pro and Enterprise differ in included compute, limits and support.",
  },
  {
    q: "What counts as a build?",
    a: "A build is a single execution of your project pipeline that produces a deployable artifact. Preview and production deployments each trigger a build.",
  },
  {
    q: "Do you offer a free tier?",
    a: "Yes. The Hobby plan costs $0 and includes $5 of compute per month, preview deployments and automatic HTTPS. It has no pay-as-you-go overage: once the $5 is used and you have no prepaid credits, new deployments are blocked until the next monthly period, unless you add credits or upgrade to Pro.",
  },
  {
    q: "Can I bring my own domain?",
    a: "Yes. Custom domains and DNS are available on every plan, with automatic HTTPS for every verified domain once its DNS points at shadw.",
  },
];

const PRIMARY_BTN =
  "tap-44 inline-flex w-full items-center justify-center gap-2 rounded-full border border-transparent bg-emerald-400 px-6 py-3 text-sm font-semibold text-black transition-transform hover:scale-[1.02]";
const OUTLINE_BTN =
  "tap-44 inline-flex w-full items-center justify-center gap-2 rounded-full border border-white/20 px-6 py-3 text-sm font-medium text-white transition-colors hover:bg-white/10";

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
        subtitle="Start free. Scale as you grow. Hobby never bills overage."
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
                    For teams
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
