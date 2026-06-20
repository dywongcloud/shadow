import Link from "next/link";
import { MarketingShell, MarketingHero, MarketingSection, GlowCard } from "@/components/marketing-shell";
import { ArrowRight } from "lucide-react";

const FEATURED = {
  title: "Introducing the Vercel-style deploy pipeline",
  excerpt:
    "Push to a branch and watch shadw fan your build out across the peer-to-peer mesh — atomic builds, immutable deployments, and preview URLs for every commit. No build servers to babysit, no cold infrastructure to provision. Here's how we made instant, global deploys feel native to a self-hosted cloud.",
  tag: "Product",
  date: "Jun 16, 2026",
  readTime: "8 min read",
  author: "Dylan Wong",
};

const POSTS: {
  title: string;
  excerpt: string;
  tag: string;
  date: string;
  readTime: string;
  author: string;
}[] = [
  {
    title: "How GuardianDB keeps data durable across a P2P mesh",
    excerpt:
      "Replication, quorum writes, and self-healing shards — a tour of the storage engine that keeps your data safe even when peers come and go.",
    tag: "Engineering",
    date: "Jun 11, 2026",
    readTime: "11 min read",
    author: "Priya Nair",
  },
  {
    title: "Anonymous preview access with zero-knowledge proofs",
    excerpt:
      "Share a private deploy without handing out credentials. We use zero-knowledge proofs to gate previews while revealing nothing about who's allowed in.",
    tag: "Security",
    date: "Jun 4, 2026",
    readTime: "9 min read",
    author: "Marco Silva",
  },
  {
    title: "Fluid compute: serverless that scales to zero",
    excerpt:
      "Functions that spin up in milliseconds on the nearest peer and disappear when idle. A look at the scheduler behind shadw's fluid compute layer.",
    tag: "Engineering",
    date: "May 28, 2026",
    readTime: "7 min read",
    author: "Dylan Wong",
  },
  {
    title: "Bring your own domain: end-to-end DNS",
    excerpt:
      "Point your domain at the mesh and shadw handles certificates, propagation, and routing automatically — with verifiable, end-to-end control of your records.",
    tag: "Product",
    date: "May 20, 2026",
    readTime: "6 min read",
    author: "Aisha Khan",
  },
  {
    title: "Instant rollback is just an alias swap",
    excerpt:
      "Every deploy is immutable, so rolling back never rebuilds anything. We just re-point an alias — and your last-known-good version is live again in seconds.",
    tag: "Engineering",
    date: "May 13, 2026",
    readTime: "5 min read",
    author: "Priya Nair",
  },
  {
    title: "Designing a control plane with no central server",
    excerpt:
      "Coordination without a coordinator. How shadw reaches consensus on deployments, leases, and routing using only the peers in your network.",
    tag: "Engineering",
    date: "May 5, 2026",
    readTime: "10 min read",
    author: "Marco Silva",
  },
];

export default function BlogPage() {
  return (
    <MarketingShell>
      <MarketingHero
        eyebrow="Blog"
        title={
          <>
            From the{" "}
            <span className="bg-gradient-to-r from-fuchsia-400 via-cyan-300 to-blue-500 bg-clip-text text-transparent">
              shadows
            </span>
          </>
        }
        subtitle="Product news, engineering deep-dives, and the peer-to-peer cloud."
      />

      <MarketingSection className="py-16">
        {/* Featured post */}
        <GlowCard>
          <div className="flex flex-col gap-4">
            <span className="w-fit rounded-full bg-white/10 px-3 py-1 text-xs text-zinc-200">
              {FEATURED.tag}
            </span>
            <h2 className="text-2xl font-bold text-white">{FEATURED.title}</h2>
            <p className="max-w-3xl text-zinc-400">{FEATURED.excerpt}</p>
            <div className="text-sm text-zinc-500">
              {FEATURED.author} &bull; {FEATURED.date} &bull; {FEATURED.readTime}
            </div>
            <Link
              href="/blog"
              className="group inline-flex w-fit items-center gap-1.5 text-sm font-medium text-emerald-400 transition-colors hover:text-emerald-300"
            >
              Read more
              <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
            </Link>
          </div>
        </GlowCard>

        {/* Post grid */}
        <div className="mt-12">
          <h3 className="text-3xl font-bold tracking-tight text-white">Latest posts</h3>
          <div className="mt-8 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
            {POSTS.map((post) => (
              <GlowCard key={post.title}>
                <Link href="/blog" className="group flex h-full flex-col gap-3">
                  <span className="w-fit rounded-full bg-white/10 px-3 py-1 text-xs text-zinc-200">
                    {post.tag}
                  </span>
                  <h4 className="text-lg font-semibold text-white transition-colors group-hover:text-emerald-300">
                    {post.title}
                  </h4>
                  <p className="line-clamp-3 text-sm text-zinc-400">{post.excerpt}</p>
                  <div className="mt-auto pt-2 text-xs text-zinc-500">
                    {post.date} &bull; {post.readTime}
                  </div>
                </Link>
              </GlowCard>
            ))}
          </div>
        </div>

        {/* Newsletter sign-up */}
        <div className="mt-12">
          <GlowCard className="text-center">
            <div className="mx-auto flex max-w-xl flex-col items-center gap-4">
              <h3 className="text-3xl font-bold tracking-tight text-white">Stay in the loop</h3>
              <p className="text-zinc-400">
                Engineering deep-dives, product launches, and the occasional peek behind the
                mesh — delivered to your inbox. No spam, unsubscribe anytime.
              </p>
              <div className="mt-2 flex w-full max-w-md flex-col items-center gap-3 sm:flex-row">
                <input
                  type="email"
                  placeholder="you@example.com"
                  className="w-full flex-1 rounded-full border border-white/15 bg-white/5 px-4 py-2.5 text-sm text-white placeholder:text-zinc-500 outline-none transition-colors focus:border-emerald-400/60"
                />
                <button
                  type="button"
                  className="w-full rounded-full bg-emerald-500 px-6 py-2.5 text-sm font-semibold text-black transition-colors hover:bg-emerald-400 sm:w-auto"
                >
                  Subscribe
                </button>
              </div>
            </div>
          </GlowCard>
        </div>
      </MarketingSection>
    </MarketingShell>
  );
}
