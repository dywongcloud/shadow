"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { ArrowLeft, Github, Search, Loader2 } from "lucide-react";
import Link from "next/link";
import { Card, Button, Input, Badge } from "@/components/ui";
import { GlobeEmptyState } from "@/components/globe";
import { apiSend } from "@/lib/api";

interface GhRepo {
  name: string;
  full_name: string;
  clone_url: string;
  default_branch: string;
  private: boolean;
}

const TEMPLATES = [
  { name: "HTML Starter", desc: "A clean static site, deployed instantly.", url: "https://github.com/mdn/beginner-html-site" },
  { name: "Hello World", desc: "The smallest possible deploy.", url: "https://github.com/octocat/Hello-World" },
  { name: "Container (Dockerfile)", desc: "Railway-style: build & run any Dockerfile.", url: "https://github.com/crccheck/docker-hello-world" },
  { name: "Static Portfolio", desc: "A single-page static portfolio.", url: "https://github.com/github/personal-website" },
];

export default function NewProjectPage() {
  const router = useRouter();
  const [url, setUrl] = useState("");
  const [deploying, setDeploying] = useState(false);
  const [error, setError] = useState("");
  const [gh, setGh] = useState<{ configured: boolean; connected: boolean }>({ configured: false, connected: false });
  const [repos, setRepos] = useState<GhRepo[]>([]);
  const [repoQ, setRepoQ] = useState("");

  useEffect(() => {
    fetch("/api/github/status").then((r) => r.json()).then((s) => {
      setGh({ configured: !!s.configured, connected: !!s.connected });
      if (s.connected) fetch("/api/github/repos").then((r) => r.json()).then((d) => setRepos(d.repos || []));
    }).catch(() => {});
  }, []);

  async function deploy(repoUrl: string, branch?: string, project?: string) {
    if (!repoUrl) return;
    setDeploying(true);
    setError("");
    try {
      const res = await apiSend<{ build_id: string }>("POST", "/v1/git/deploy", {
        repo_url: repoUrl,
        branch,
        project,
        creator: "you",
      });
      // Stream the build logs on the deploy screen.
      router.push(`/deploy/${res.build_id}`);
    } catch (e) {
      setError(String(e));
      setDeploying(false);
    }
  }

  async function connectGithub() {
    const r = await fetch("/api/github/connect", { method: "POST" });
    const d = await r.json();
    if (d.redirectUrl) window.location.href = d.redirectUrl;
    else setError(d.error || "Could not start GitHub connection");
  }

  const filteredRepos = repos.filter((r) =>
    r.full_name.toLowerCase().includes(repoQ.toLowerCase())
  );

  return (
    <div className="mx-auto max-w-4xl">
      <Link href="/" className="mb-6 inline-flex items-center gap-2 text-sm text-secondary hover:text-fg">
        <ArrowLeft className="h-4 w-4" /> Back
      </Link>
      <h1 className="mb-6 text-3xl font-semibold tracking-tight">Let&apos;s build something new</h1>

      {/* Git URL bar */}
      <Card className="mb-2 p-2">
        <div className="flex items-center gap-2">
          <Input
            placeholder="Enter a Git repository URL to deploy…"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && deploy(url)}
            className="border-0 focus:ring-0"
          />
          <Button onClick={() => deploy(url)} disabled={deploying || !url}>
            {deploying ? <Loader2 className="h-4 w-4 animate-spin" /> : "Deploy"}
          </Button>
        </div>
      </Card>
      <p className="mb-8 text-center text-sm text-muted">
        Paste any public Git repo URL — Hive clones, builds, and deploys it.
      </p>
      {error ? <p className="mb-6 text-center text-sm text-red-600">{error}</p> : null}

      <div className="grid grid-cols-1 gap-8 md:grid-cols-2">
        {/* Import Git Repository */}
        <div>
          <h2 className="mb-4 text-lg font-semibold">Import Git Repository</h2>
          <Card className="p-4">
            {gh.connected ? (
              <>
                <div className="relative mb-3">
                  <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
                  <Input placeholder="Search…" value={repoQ} onChange={(e) => setRepoQ(e.target.value)} className="pl-9" />
                </div>
                <div className="flex max-h-80 flex-col divide-y divide-border overflow-y-auto">
                  {filteredRepos.map((r) => (
                    <div key={r.full_name} className="flex items-center justify-between py-2.5">
                      <div className="flex items-center gap-2 text-sm">
                        <Github className="h-4 w-4 text-secondary" />
                        <span className="font-medium">{r.name}</span>
                        {r.private ? <Badge>private</Badge> : null}
                      </div>
                      <Button onClick={() => deploy(r.clone_url, r.default_branch, r.name)} disabled={deploying}>
                        Import
                      </Button>
                    </div>
                  ))}
                  {!filteredRepos.length && <div className="py-6 text-center text-sm text-secondary">No repositories.</div>}
                </div>
              </>
            ) : (
              <div className="flex flex-col items-center gap-3 py-8 text-center">
                <Github className="h-8 w-8" />
                <div className="font-medium">Connect GitHub</div>
                <p className="max-w-xs text-sm text-secondary">
                  {gh.configured
                    ? "Authorize GitHub to import and deploy your repositories."
                    : "Set COMPOSIO_API_KEY to enable multi-tenant GitHub OAuth. You can still deploy any public repo by URL above."}
                </p>
                {gh.configured && (
                  <Button onClick={connectGithub}>
                    <Github className="h-4 w-4" /> Connect GitHub
                  </Button>
                )}
              </div>
            )}
          </Card>
        </div>

        {/* Clone Template */}
        <div>
          <h2 className="mb-4 text-lg font-semibold">Clone Template</h2>
          <div className="grid grid-cols-1 gap-3">
            {TEMPLATES.map((t) => (
              <Card key={t.name} className="flex items-center justify-between p-4">
                <div>
                  <div className="font-medium">{t.name}</div>
                  <div className="text-sm text-secondary">{t.desc}</div>
                </div>
                <Button variant="outline" onClick={() => deploy(t.url, undefined, t.name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, ""))} disabled={deploying}>
                  Clone
                </Button>
              </Card>
            ))}
          </div>
        </div>
      </div>

      {/* Deployment progress card — always sits below the configuration cards. */}
      <Card className="mt-8 overflow-hidden p-6 sm:p-8">
        <h2 className="text-2xl font-semibold tracking-tight">Deployment</h2>
        <p className="mt-1.5 text-sm text-secondary">
          {deploying ? "Starting your deployment…" : "Once you're ready, start deploying to see the progress here…"}
        </p>
        <div className="pointer-events-none mt-6 flex justify-center">
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <GlobeOnly />
        </div>
      </Card>
    </div>
  );
}

/** Just the globe wireframe graphic (no heading), themed for light/dark. */
function GlobeOnly() {
  return <GlobeEmptyState title="" desc={undefined} />;
}
