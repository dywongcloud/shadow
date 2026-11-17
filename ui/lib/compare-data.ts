/** Comparison pages. Every shadw statement here is backed by ui/app/features/page.tsx,
 *  ui/app/pricing/page.tsx, ui/app/layout.tsx or crates/hive-cloud/src/billing.rs.
 *  Every rival statement was checked against that provider's own published docs on
 *  17 November 2026; the sources are printed on the page with the retrieval date.
 *  Competitor capabilities are described, not denied: a rival feature that exists is
 *  never presented as missing just to make shadw look better. */

export type Comparison = {
  slug: string;
  rival: string;
  /** Metadata: `pageMeta` uses these verbatim for title, canonical, og:url and description. */
  title: string;
  socialTitle: string;
  description: string;
  twitterDescription?: string;
  /** One line on the /compare index card. */
  summary: string;
  /** The h1 is "shadw vs. <rival>." (faded) followed by this statement. */
  heroLead: string;
  heroSub: string;
  approachesLead: string;
  rivalApproach: string;
  shadwApproach: string;
  differencesLead: string;
  differences: { title: string; body: string }[];
  rows: { label: string; shadw: string; rival: string }[];
  fit: string[];
  asOf: string;
  sources: { label: string; href: string }[];
};

const AS_OF = "17 November 2026";

const SHADW_APPROACH =
  "Connect a repository and shadw detects the framework, builds an immutable deployment and serves it at a live URL. A connected pull request gets its own preview, and managed Postgres, Redis and SQLite databases are available in the same project as the app.";

const SELF_HOST_DIFFERENCE = {
  title: "Your machines are an option",
  body:
    "The hosted service is one way to run shadw. The platform also runs on your own nodes, so a team that needs its own infrastructure keeps the same Git-to-URL workflow instead of changing tools.",
};

const FREE_START_DIFFERENCE = {
  title: "Previews and HTTPS are in the free tier",
  body:
    "Hobby is a no-payment tier, and preview deployments, automatic HTTPS and custom domains with DNS are part of it. Paid tiers raise the project, member and compute allowances rather than adding the basics.",
};

export const VERCEL: Comparison = {
    slug: "vercel",
    rival: "Vercel",
    title: "shadw vs Vercel",
    socialTitle: "shadw vs Vercel — a Vercel alternative with managed data",
    description:
      "How shadw compares with Vercel: Git deploys, preview URLs, managed Postgres, Redis and SQLite, Dockerfile services, and the option to run on your own machines. With sources.",
    twitterDescription:
      "Git deploys, previews and managed data, with the option to self-host. See how shadw compares with Vercel.",
    summary:
      "Both turn a Git push into a live preview. Vercel adds databases through Marketplace providers; shadw provisions them in the project and can run on your own nodes.",
    heroLead:
      "Vercel turns a Git push into a live URL with a preview for every change. shadw keeps that workflow, provisions the databases in the same project, and can run on your own machines.",
    heroSub:
      "Both start from a Git repository and give every change a URL worth sharing. The differences are where the data comes from and where the platform can run.",
    approachesLead:
      "Vercel is a managed frontend cloud with a deep framework integration. shadw is an app platform you can host or run yourself.",
    rivalApproach:
      "Connect a Git repository and Vercel deploys automatically on branch pushes and production merges. A pull request gets a unique preview deployment with its own generated URL. Functions handle each incoming request as a scaled invocation, and Postgres, KV and NoSQL databases are provisioned through Vercel Marketplace providers such as Neon and Upstash.",
    shadwApproach: SHADW_APPROACH,
    differencesLead: "Four differences you will notice on your first deploy.",
    differences: [
      SELF_HOST_DIFFERENCE,
      {
        title: "The database is part of the project",
        body:
          "Managed Postgres, Redis and SQLite are provisioned in the same project as the deployment. With Vercel, the documented path is to provision a database through a Marketplace provider and connect it to the project.",
      },
      {
        title: "A Dockerfile deploys next to a function",
        body:
          "Long-running services and custom runtimes ship from a Dockerfile alongside serverless functions in one project. Production stays warm while previews scale to zero when idle.",
      },
      FREE_START_DIFFERENCE,
    ],
    rows: [
      { label: "Deploy trigger", shadw: "Push to a connected Git repository", rival: "Automatic deploys on branch pushes and production merges" },
      { label: "Preview per change", shadw: "A live URL for every pull request on a connected GitHub repository", rival: "A unique preview deployment and URL for each pull request" },
      { label: "Managed databases", shadw: "Postgres, Redis and SQLite in the same project", rival: "Postgres, KV and NoSQL through Vercel Marketplace providers" },
      { label: "Long-running services", shadw: "Dockerfile services alongside functions", rival: "Request-driven Vercel Functions and build outputs" },
      { label: "Rollback", shadw: "Promote any earlier immutable deployment", rival: "Instant rollbacks when reverting changes on a custom domain" },
      { label: "Custom domains and TLS", shadw: "Managed DNS, automatic TLS, wildcard preview subdomains", rival: "Automatic certificates for added domains" },
      { label: "Where it runs", shadw: "Hosted service or your own nodes", rival: "Vercel's managed cloud, with configurable function regions" },
      { label: "Pricing model", shadw: "No-payment Hobby tier, usage-based paid plans", rival: "Free Hobby plan, Pro with included usage credit, pay as you go" },
    ],
    fit: [
      "You want the database and the app in one project instead of connecting a Marketplace provider.",
      "You need a long-running Dockerfile service next to your serverless functions.",
      "Your team may have to run the whole platform on its own machines later.",
      "You want previews, HTTPS and custom domains included in a no-payment starting tier.",
    ],
    asOf: AS_OF,
    sources: [
      { label: "Vercel Git deployments", href: "https://vercel.com/docs/git" },
      { label: "Vercel preview deployments", href: "https://vercel.com/docs/deployments/preview-deployments" },
      { label: "Vercel storage", href: "https://vercel.com/docs/storage" },
      { label: "Vercel pricing", href: "https://vercel.com/pricing" },
      { label: "shadw features", href: "/features" },
      { label: "shadw pricing", href: "/pricing" },
    ],
  };

export const RAILWAY: Comparison = {
    slug: "railway",
    rival: "Railway",
    title: "shadw vs Railway",
    socialTitle: "shadw vs Railway — a Railway alternative with a free tier and self-hosting",
    description:
      "How shadw compares with Railway: Git deploys, PR environments, managed data, immutable rollback and the option to run on your own machines. With sources.",
    twitterDescription:
      "Git deploys, previews, managed data and the option to self-host. See how shadw compares with Railway.",
    summary:
      "Both deploy a repository or a Dockerfile with managed databases and PR environments. shadw adds a no-payment tier, serverless functions and the option to run on your own nodes.",
    heroLead:
      "Railway deploys a repository or a Dockerfile with managed databases and isolated PR environments. shadw covers the same ground, with serverless functions, a no-payment tier and the option to run on your own machines.",
    heroSub:
      "Both put an app online without a server to provision, and both can give a pull request its own environment. The differences are the release workflow and where the platform can run.",
    approachesLead:
      "Railway runs services and databases as billable resources. shadw carries the release workflow in the platform.",
    rivalApproach:
      "Deploy from GitHub, the CLI, a Docker image or a template; Railway detects a Dockerfile at the root of the source directory. PostgreSQL and Redis are available as zero-configuration database templates. PR environments provision isolated, temporary infrastructure when a pull request opens and remove it when the pull request closes, and services can sleep when inactive.",
    shadwApproach: SHADW_APPROACH,
    differencesLead: "Four differences you will notice on your first deploy.",
    differences: [
      FREE_START_DIFFERENCE,
      SELF_HOST_DIFFERENCE,
      {
        title: "Functions next to services",
        body:
          "Serverless functions deploy alongside Dockerfile services in one project. Production stays warm and previews scale to zero when idle, so an idle preview does not need a running container.",
      },
      {
        title: "Rollback is a promotion",
        body:
          "Every build is an immutable, content-addressed deployment. Recovering from a bad release means promoting an earlier deployment back to production, not rebuilding the previous commit.",
      },
    ],
    rows: [
      { label: "Deploy trigger", shadw: "Push to a connected Git repository, or a Dockerfile", rival: "GitHub, CLI, Docker image or template" },
      { label: "Preview per pull request", shadw: "A live URL for every pull request on a connected GitHub repository", rival: "Isolated, temporary PR environments when enabled" },
      { label: "Managed databases", shadw: "Postgres, Redis and SQLite in the same project", rival: "PostgreSQL and Redis zero-configuration templates" },
      { label: "Serverless functions", shadw: "Production warm, previews scale to zero", rival: "Serverless applied to a container; inactive services sleep" },
      { label: "Rollback", shadw: "Promote any earlier immutable deployment", rival: "Redeploy an earlier deployment" },
      { label: "Custom domains and TLS", shadw: "Managed DNS, automatic TLS, wildcard preview subdomains", rival: "Railway-provided domains, custom domains, automatic SSL" },
      { label: "Where it runs", shadw: "Hosted service or your own nodes", rival: "Railway's managed cloud, with selectable regions" },
      { label: "Pricing model", shadw: "No-payment Hobby tier, usage-based paid plans", rival: "Subscription plus metered usage by the second" },
    ],
    fit: [
      "You want a no-payment starting tier with previews, HTTPS and custom domains included.",
      "You want serverless functions and a long-running service in the same project.",
      "You want a bad release to be a promotion of an earlier immutable deployment.",
      "You may need to run the whole platform on your own machines later.",
    ],
    asOf: AS_OF,
    sources: [
      { label: "Railway deployments", href: "https://docs.railway.com/deployments" },
      { label: "Railway PR environments", href: "https://railway.com/guides/preview-deployments-with-pr-environments" },
      { label: "Railway PostgreSQL", href: "https://docs.railway.com/databases/postgresql" },
      { label: "Railway serverless", href: "https://docs.railway.com/deployments/serverless" },
      { label: "Railway pricing", href: "https://railway.com/pricing" },
      { label: "shadw features", href: "/features" },
    ],
  };

export const FLY_IO: Comparison = {
    slug: "fly-io",
    rival: "Fly.io",
    title: "shadw vs Fly.io",
    socialTitle: "shadw vs Fly.io — a Fly.io alternative with Git deploys and previews",
    description:
      "How shadw compares with Fly.io: Git push to a live URL instead of a CLI launch, preview per pull request, managed data, and the option to run on your own machines. With sources.",
    twitterDescription:
      "Git push to a live URL, a preview per pull request and managed data, with the option to self-host. See how shadw compares with Fly.io.",
    summary:
      "Fly.io gives you Machines, regions and volumes with fine-grained control. shadw starts at a Git push and carries the framework, previews and managed databases for you.",
    heroLead:
      "Fly.io gives you fast-booting Machines in regions you choose, with a Dockerfile and a CLI. shadw starts at a Git push: framework detection, a live URL, a preview per pull request and the databases in the same project.",
    heroSub:
      "Both can run a real container. The difference is how much of the deployment you configure versus how much the platform does for you.",
    approachesLead:
      "Fly.io is infrastructure you assemble with a launch command and a config file. shadw is a deploy workflow that detects the app and builds it.",
    rivalApproach:
      "Describe the app in a Dockerfile, or let fly launch generate one for common frameworks, then fly deploy builds the image and refreshes Machines. Git-driven deployment is wired through a GitHub Actions workflow, and ephemeral per-pull-request review apps are available as a GitHub Actions blueprint. Fly.io also offers a fully managed Postgres service.",
    shadwApproach: SHADW_APPROACH,
    differencesLead: "Four differences you will notice on your first deploy.",
    differences: [
      {
        title: "Git push is the deploy command",
        body:
          "Connect a repository and shadw detects the framework and builds on push. There is no launch command, machine size or region to choose before the first URL exists.",
      },
      {
        title: "Previews come from the connected repository",
        body:
          "A connected GitHub pull request gets its own live URL, with wildcard preview subdomains and automatic TLS when you bring a custom domain. On Fly.io, per-pull-request review apps are a GitHub Actions blueprint you set up.",
      },
      {
        title: "Three managed database kinds in the project",
        body:
          "Managed Postgres, Redis and SQLite are all available in the same project as the deployment. Fly.io's documented first-party managed database service is Postgres.",
      },
      SELF_HOST_DIFFERENCE,
    ],
    rows: [
      { label: "First deploy", shadw: "Connect a repository and push", rival: "Dockerfile, fly launch and fly deploy" },
      { label: "Preview per pull request", shadw: "A live URL for every connected GitHub pull request", rival: "Ephemeral review apps via a GitHub Actions blueprint" },
      { label: "Framework detection", shadw: "Next.js, Vite, SvelteKit and others detected on build", rival: "fly launch generates a Dockerfile for common frameworks" },
      { label: "Managed databases", shadw: "Postgres, Redis and SQLite in the same project", rival: "Fly.io Managed Postgres" },
      { label: "Long-running services", shadw: "Dockerfile services alongside functions", rival: "Fly Machines, with regions and volumes you choose" },
      { label: "Custom domains and TLS", shadw: "Managed DNS, automatic TLS, wildcard preview subdomains", rival: "Automatic Let's Encrypt certificates for custom domains" },
      { label: "Where it runs", shadw: "Hosted service or your own nodes", rival: "Fly.io's managed cloud, with Machines in multiple regions" },
      { label: "Pricing model", shadw: "No-payment Hobby tier, usage-based paid plans", rival: "Usage-based, with a free starting credit" },
    ],
    fit: [
      "You would rather push code than maintain a Dockerfile, machine sizes and region placement.",
      "You want every pull request to have a URL without setting up a deployment workflow.",
      "You want Redis or SQLite provisioned alongside Postgres in the app's project.",
      "You may need to run the same platform on your own machines later.",
    ],
    asOf: AS_OF,
    sources: [
      { label: "Fly.io deploy", href: "https://docs.fly.io/launch/deploy" },
      { label: "Fly.io launch", href: "https://docs.fly.io/launch/create" },
      { label: "Fly.io review apps", href: "https://docs.fly.io/blueprints/review-apps-guide" },
      { label: "Fly.io Managed Postgres", href: "https://docs.fly.io/postgres" },
      { label: "Fly.io Machines", href: "https://fly.io/machines/" },
      { label: "Fly.io pricing", href: "https://fly.io/pricing/" },
      { label: "shadw features", href: "/features" },
    ],
  };

export const CLOUDFLARE: Comparison = {
    slug: "cloudflare",
    rival: "Cloudflare",
    title: "shadw vs Cloudflare",
    socialTitle: "shadw vs Cloudflare — a Cloudflare alternative for full app deployments",
    description:
      "How shadw compares with Cloudflare Workers, Pages and D1: Git deploys for framework apps, a preview per pull request, Postgres and Redis alongside SQLite, and the option to run on your own machines. With sources.",
    twitterDescription:
      "Full framework apps from a Git push, previews, managed data and the option to self-host. See how shadw compares with Cloudflare.",
    summary:
      "Cloudflare runs code on its global network with Workers, Pages and D1. shadw deploys a whole framework app from Git and puts Postgres, Redis and SQLite in the same project.",
    heroLead:
      "Cloudflare runs code across a global network with Workers, Pages and D1. shadw deploys a whole framework app from a Git push, with a preview per pull request and managed Postgres, Redis and SQLite in the same project.",
    heroSub:
      "Both serve an app without a server to manage, and both give a pull request a preview URL. The difference is the runtime your code targets and how much of the app the platform builds for you.",
    approachesLead:
      "Cloudflare is an edge platform built around Workers, Pages and its own data services. shadw is a Git-driven deploy platform for framework apps and containers.",
    rivalApproach:
      "Connect a Git provider to Pages, use Direct Upload, or deploy Workers with the CLI. Every new pull request gets a unique preview URL that stays updated as commits arrive, and rollbacks instantly revert to a previous production deployment. Workers is a serverless execution environment, and D1 is a managed serverless database with SQLite's SQL semantics; KV, R2 and Durable Objects cover other data needs.",
    shadwApproach: SHADW_APPROACH,
    differencesLead: "Four differences you will notice on your first deploy.",
    differences: [
      {
        title: "Framework apps and containers are the deploy unit",
        body:
          "shadw detects the framework and builds the app, and a Dockerfile can ship a long-running service alongside functions. Cloudflare's documented primary platform for new projects is Workers, with Pages for connected Git repositories.",
      },
      {
        title: "Postgres and Redis alongside SQLite",
        body:
          "Managed Postgres, Redis and SQLite are all available in the same project. Cloudflare's documented managed database is D1, with SQLite's SQL semantics, plus KV, R2 and Durable Objects for other data shapes.",
      },
      {
        title: "A preview per connected pull request",
        body:
          "A connected GitHub pull request gets its own live URL, with wildcard preview subdomains and automatic TLS when you bring a custom domain. Pages also creates a preview URL per pull request, so this is a shared strength rather than a gap.",
      },
      SELF_HOST_DIFFERENCE,
    ],
    rows: [
      { label: "Deploy trigger", shadw: "Push to a connected Git repository", rival: "Connected Git provider, Direct Upload or CLI" },
      { label: "Preview per pull request", shadw: "A live URL for every connected GitHub pull request", rival: "A unique preview URL per pull request, updated on each commit" },
      { label: "Runtime", shadw: "Framework apps, serverless functions and Dockerfile services", rival: "Workers serverless environment and Pages Functions" },
      { label: "Managed databases", shadw: "Postgres, Redis and SQLite in the same project", rival: "D1 SQLite, plus KV, R2 and Durable Objects" },
      { label: "Custom domains and TLS", shadw: "Managed DNS, automatic TLS, wildcard preview subdomains", rival: "Cloudflare-issued certificates for custom domains" },
      { label: "Rollback", shadw: "Promote any earlier immutable deployment", rival: "Instant rollback to a previous production deployment" },
      { label: "Where it runs", shadw: "Hosted service or your own nodes", rival: "Cloudflare's global network" },
      { label: "Pricing model", shadw: "No-payment Hobby tier, usage-based paid plans", rival: "Workers Free plan, then a usage-based paid plan" },
    ],
    fit: [
      "Your app is a framework app or a container you want to deploy as it is, rather than reshape around a serverless runtime.",
      "You want Postgres or Redis provisioned with the app alongside a SQLite database.",
      "You want every pull request to have a URL that reviewers can open immediately.",
      "You may need to run the same platform on your own machines later.",
    ],
    asOf: AS_OF,
    sources: [
      { label: "Cloudflare Workers", href: "https://developers.cloudflare.com/workers/" },
      { label: "Cloudflare Pages", href: "https://developers.cloudflare.com/pages/" },
      { label: "Cloudflare preview deployments", href: "https://developers.cloudflare.com/pages/configuration/preview-deployments/" },
      { label: "Cloudflare D1", href: "https://developers.cloudflare.com/d1/" },
      { label: "Workers pricing", href: "https://developers.cloudflare.com/workers/platform/pricing/" },
      { label: "shadw features", href: "/features" },
    ],
  };


/** The index, nav and footer iterate this one array. */
export const COMPARISONS: Comparison[] = [VERCEL, RAILWAY, FLY_IO, CLOUDFLARE];
