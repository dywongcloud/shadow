import type { Metadata } from "next";
import { darkViewport, pageMeta } from "@/lib/page-meta";
import { MarketingShell, MarketingHero, MarketingSection } from "@/components/marketing-shell";

export const metadata: Metadata = pageMeta({
  title: "Privacy Policy",
  socialTitle: "Privacy Policy — shadw",
  description:
    "shadw's privacy policy: what the hosted dashboard and managed services collect, where your application data is stored, what platform operators can read, our use of first-party cookies, and what happens when you delete a resource.",
  twitterDescription:
    "shadw's privacy policy: what the hosted dashboard collects, where your application data is stored, what platform operators can read, and what happens when you delete a resource.",
  path: "/privacy",
});
export const viewport = darkViewport;


const SECTIONS: { h: string; p: string }[] = [
  { h: "Overview", p: "shadw is a cloud you can self-host. When you self-host, your application data, deployments, and durable storage live on nodes you operate. On the hosted service they live on nodes run by the shadw operators. This policy describes what the hosted dashboard and managed services collect." },
  { h: "Data we collect", p: "Account details (name, email, organization) for authentication; project and deployment metadata to run the dashboard; and page-load performance measurements (Core Web Vitals) that a first-party beacon sends together with the URL of the page. Environment variables marked sensitive, and values that look like credentials, are encrypted at rest." },
  { h: "Your application data", p: "Deployments, databases, and durable data are stored on the nodes that run them. On the hosted service, platform operators can read platform records across all tenants through operator-only admin routes, including projects, deployments, databases, domains, workflow runs, request events, billing, and audit entries." },
  { h: "DNS & domains", p: "When you bring a domain into the DNS console, we store the records you configure to answer DNS queries and to attach domains to deployments. You can import and delete these records at any time." },
  { h: "Cookies & sessions", p: "We use first-party cookies strictly to keep you signed in and to remember your active team. No third-party advertising trackers." },
  { h: "Data retention & deletion", p: "You can delete projects, deployments, and individual DNS records, and detach domains from projects, from the dashboard. Account deletion is not implemented in the dashboard. Deleting a resource removes the resource itself; project tombstones (the project name and deletion time) and the append-only audit history persist, and request-event history is kept for 30 days by default. Self-hosted data is removed when you remove it from your nodes." },
  { h: "Contact", p: "Questions about privacy or a data request? Email privacy@shadw.cloud." },
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
        subtitle="Your cloud, your data. Last updated October 14, 2026."
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
