import type { Metadata } from "next";
import { MarketingShell, MarketingHero, MarketingSection } from "@/components/marketing-shell";

// Perf: prerender this public page (see root layout note).
export const dynamic = "force-static";
export const revalidate = 3600;

export const metadata: Metadata = {
  title: "Privacy Policy — autheo",
  description:
    "autheo's privacy policy: what the hosted dashboard and managed services collect, how your self-hosted application data and encrypted secrets stay on nodes you operate, our use of first-party cookies, and how to export or delete your data.",
  alternates: { canonical: "/privacy" },
  openGraph: {
    title: "Privacy Policy — autheo",
    description:
      "autheo's privacy policy: what the hosted dashboard and managed services collect, how your self-hosted application data and encrypted secrets stay on nodes you operate, our use of first-party cookies, and how to export or delete your data.",
    url: "/privacy",
    type: "website",
    siteName: "autheo",
  },
  twitter: {
    card: "summary_large_image",
    title: "Privacy Policy — autheo",
    description:
      "autheo's privacy policy: what the hosted dashboard collects, how your self-hosted data and encrypted secrets stay on your own nodes, and how to export or delete your data.",
  },
};


const SECTIONS: { h: string; p: string }[] = [
  { h: "Overview", p: "autheo is a self-hosted, peer-to-peer cloud. When you self-host, your application data, deployments, and durable storage live on nodes you operate — not ours. This policy describes what the hosted dashboard and managed services collect." },
  { h: "Data we collect", p: "Account details (name, email, organization) for authentication; project and deployment metadata to run the dashboard; and aggregate, non-identifying usage metrics to improve the product. Environment variables and secrets are encrypted at rest and never logged in plaintext." },
  { h: "Your application data", p: "Deployments, databases (GuardianDB), and durable data are stored on the nodes in your mesh. On a self-hosted cluster, that data never leaves your infrastructure. We do not read, sell, or share your application data." },
  { h: "DNS & domains", p: "When you bring a domain into the DNS console, we store the records you configure to answer DNS queries and to attach domains to deployments. You can export or delete these records at any time." },
  { h: "Cookies & sessions", p: "We use first-party cookies strictly to keep you signed in and to remember your active team. No third-party advertising trackers." },
  { h: "Data retention & deletion", p: "You can delete projects, deployments, domains, and your account at any time from the dashboard. Deleting a resource removes its metadata from our systems; self-hosted data is removed when you remove it from your nodes." },
  { h: "Contact", p: "Questions about privacy or a data request? Email privacy@autheo.dev and we'll respond promptly." },
];

export default function PrivacyPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Legal"
        title={
          <>
            Privacy <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">Policy</span>
          </>
        }
        subtitle="Your cloud, your data. Last updated June 2026."
      />
      <MarketingSection className="pb-24">
        <div className="mx-auto max-w-3xl space-y-8">
          {SECTIONS.map((s) => (
            <div key={s.h}>
              <h2 className="text-xl font-semibold text-white">{s.h}</h2>
              <p className="mt-2 leading-relaxed text-zinc-400">{s.p}</p>
            </div>
          ))}
        </div>
      </MarketingSection>
    </MarketingShell>
  );
}
