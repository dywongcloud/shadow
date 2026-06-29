"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { GitBranch, GitCommit, Plus, Download, Upload, KeyRound, FolderGit2, RefreshCw, FileCode2, Cloud, HardDrive } from "lucide-react";
import { Card, Button, Input, PageHeader, Badge } from "@/components/ui";
import {
  cloneRepo,
  initRepo,
  writeFile,
  commitAll,
  push,
  status as gitStatus,
  log as gitLog,
  type FileStatus,
} from "@/lib/isogit";
import { currentTeam } from "@/lib/api";
import {
  syncLocalGitops,
  readLocalSnapshot,
  localProviderActive,
  localGitLog,
  type LocalSnapshot,
} from "@/lib/gitops-local";

// Client-side git in the browser (isomorphic-git + LightningFS). The PAT is held
// only in this tab (localStorage) and is sent over our own same-origin proxy —
// never to a third party. This page is fully additive: it doesn't touch the
// deploy / GitOps-sync flows.
export default function GitPage() {
  const [pat, setPat] = useState("");
  const [author, setAuthor] = useState({ name: "", email: "" });
  const [out, setOut] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    setPat(localStorage.getItem("hive_git_pat") || "");
    setAuthor({
      name: localStorage.getItem("hive_git_name") || "",
      email: localStorage.getItem("hive_git_email") || "",
    });
  }, []);
  useEffect(() => {
    logRef.current?.scrollTo(0, logRef.current.scrollHeight);
  }, [out]);

  const say = (s: string) => setOut((o) => [...o, s]);
  function saveCreds() {
    localStorage.setItem("hive_git_pat", pat);
    localStorage.setItem("hive_git_name", author.name);
    localStorage.setItem("hive_git_email", author.email);
    say("Saved credentials in this browser.");
  }

  async function run(fn: () => Promise<void>) {
    if (busy) return;
    setBusy(true);
    try {
      await fn();
    } catch (e) {
      say(`Error: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mx-auto max-w-[1100px]">
      <PageHeader
        title="Git"
        desc="Client-side git in your browser — clone, commit, push, and create repos with isomorphic-git. No server-side checkout."
      />

      <GitOpsMirrorCard />

      <div className="grid grid-cols-1 gap-5 lg:grid-cols-2">
        {/* Credentials */}
        <Card className="p-5">
          <div className="mb-3 flex items-center gap-2 font-semibold">
            <KeyRound className="h-4 w-4" /> Credentials
          </div>
          <p className="mb-3 text-xs text-secondary">
            Optional — if your GitHub account is connected (Integrations), clone/push/create use that connection
            automatically. Otherwise paste a personal access token (repo scope). A PAT is stored only in this browser;
            either way credentials are sent over the dashboard&apos;s own proxy.
          </p>
          <div className="flex flex-col gap-2">
            <Field label="GitHub token (PAT) — optional">
              <Input type="password" value={pat} onChange={(e) => setPat(e.target.value)} placeholder="ghp_… (blank = use connected GitHub)" className="font-mono" />
            </Field>
            <div className="grid grid-cols-2 gap-2">
              <Field label="Author name">
                <Input value={author.name} onChange={(e) => setAuthor({ ...author, name: e.target.value })} placeholder="Ada Lovelace" />
              </Field>
              <Field label="Author email">
                <Input value={author.email} onChange={(e) => setAuthor({ ...author, email: e.target.value })} placeholder="ada@example.com" />
              </Field>
            </div>
            <Button onClick={saveCreds} variant="outline" className="mt-1 self-start">Save in browser</Button>
          </div>
        </Card>

        {/* Output log */}
        <Card className="p-5">
          <div className="mb-3 flex items-center gap-2 font-semibold">
            <GitCommit className="h-4 w-4" /> Activity
            {busy && <Badge tone="blue">working…</Badge>}
          </div>
          <div ref={logRef} className="h-44 overflow-auto rounded-md border border-border bg-bg p-3 font-mono text-xs text-secondary">
            {out.length === 0 ? <span className="text-muted">No activity yet.</span> : out.map((l, i) => <div key={i}>{l}</div>)}
          </div>
        </Card>

        <NewRepoCard pat={pat} author={author} say={say} run={run} busy={busy} />
        <ExistingRepoCard pat={pat} author={author} say={say} run={run} busy={busy} />
      </div>
    </div>
  );
}

function NewRepoCard({
  pat, author, say, run, busy,
}: { pat: string; author: { name: string; email: string }; say: (s: string) => void; run: (f: () => Promise<void>) => void; busy: boolean }) {
  const [name, setName] = useState("");
  const [isPrivate, setIsPrivate] = useState(true);
  const [readme, setReadme] = useState("# New repo\n\nCreated from the dashboard.\n");

  function create() {
    run(async () => {
      if (!name.trim()) throw new Error("Repo name is required.");
      if (!author.name || !author.email) throw new Error("Set author name + email first.");
      say(`Creating GitHub repo "${name}" …`);
      let cloneUrl: string;
      let defaultBranch: string;
      let htmlUrl: string;
      if (pat) {
        // PAT path (unchanged): GitHub REST API sends CORS headers, so this runs
        // directly from the browser.
        const res = await fetch("https://api.github.com/user/repos", {
          method: "POST",
          headers: { Authorization: `Bearer ${pat}`, Accept: "application/vnd.github+json", "content-type": "application/json" },
          body: JSON.stringify({ name: name.trim(), private: isPrivate, auto_init: false }),
        });
        if (!res.ok) throw new Error(`GitHub: ${res.status} ${(await res.json().catch(() => ({}))).message || ""}`);
        const repo = await res.json();
        cloneUrl = repo.clone_url;
        defaultBranch = repo.default_branch || "main";
        htmlUrl = repo.html_url;
      } else {
        // No PAT → create via this user's connected GitHub account (Composio),
        // server-side. The push below authenticates through the same connection
        // via the git proxy, so no token is needed in the browser.
        const res = await fetch("/api/github/create-repo", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ name: name.trim(), isPrivate }),
        });
        const data = await res.json().catch(() => ({}));
        if (!res.ok || !data?.ok) {
          throw new Error(data?.error || "Create failed. Connect GitHub in Integrations, or set a PAT.");
        }
        cloneUrl = data.clone_url;
        defaultBranch = data.default_branch || "main";
        htmlUrl = data.html_url;
      }
      say(`Created ${htmlUrl.replace("https://github.com/", "")}. Initializing locally…`);
      const dir = await initRepo(name);
      await writeFile(dir, "README.md", readme);
      const sha = await commitAll(dir, "Initial commit", author);
      say(`Committed ${sha.slice(0, 8)}. Pushing…`);
      // token: pat may be empty — the proxy injects the Composio connection's token.
      await push({ dir, url: cloneUrl, token: pat, ref: defaultBranch, onProgress: say });
      say(`Done → ${htmlUrl}`);
    });
  }

  return (
    <Card className="p-5">
      <div className="mb-3 flex items-center gap-2 font-semibold">
        <Plus className="h-4 w-4" /> New repository
      </div>
      <div className="flex flex-col gap-2">
        <Field label="Name">
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="my-new-app" className="font-mono" />
        </Field>
        <label className="flex items-center gap-2 text-sm">
          <input type="checkbox" checked={isPrivate} onChange={(e) => setIsPrivate(e.target.checked)} /> Private
        </label>
        <Field label="README.md">
          <textarea
            value={readme}
            onChange={(e) => setReadme(e.target.value)}
            rows={4}
            className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-xs focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
          />
        </Field>
        <Button onClick={create} disabled={busy} className="mt-1 self-start">
          <Plus className="mr-1.5 h-3.5 w-3.5" /> Create + push
        </Button>
      </div>
    </Card>
  );
}

function ExistingRepoCard({
  pat, author, say, run, busy,
}: { pat: string; author: { name: string; email: string }; say: (s: string) => void; run: (f: () => Promise<void>) => void; busy: boolean }) {
  const [url, setUrl] = useState("");
  const [dir, setDir] = useState("");
  const [path, setPath] = useState("");
  const [content, setContent] = useState("");
  const [message, setMessage] = useState("");
  const [changes, setChanges] = useState<FileStatus[]>([]);
  const [commits, setCommits] = useState<Array<{ oid: string; message: string; author: string; ts: number }>>([]);

  function clone() {
    run(async () => {
      if (!url.trim()) throw new Error("Repo URL is required.");
      const d = await cloneRepo({ url: url.trim(), token: pat, onProgress: say });
      setDir(d);
      setCommits(await gitLog(d));
      setChanges(await gitStatus(d));
    });
  }
  function stageWrite() {
    run(async () => {
      if (!dir) throw new Error("Clone a repo first.");
      if (!path.trim()) throw new Error("File path is required.");
      await writeFile(dir, path.trim(), content);
      say(`Wrote ${path}`);
      setChanges(await gitStatus(dir));
    });
  }
  function commitPush() {
    run(async () => {
      if (!dir) throw new Error("Clone a repo first.");
      if (!author.name || !author.email) throw new Error("Set author name + email first.");
      if (!message.trim()) throw new Error("Commit message is required.");
      const sha = await commitAll(dir, message.trim(), author);
      say(`Committed ${sha.slice(0, 8)}. Pushing…`);
      await push({ dir, token: pat, onProgress: say });
      setMessage("");
      setChanges(await gitStatus(dir));
      setCommits(await gitLog(dir));
    });
  }

  return (
    <Card className="p-5">
      <div className="mb-3 flex items-center gap-2 font-semibold">
        <GitBranch className="h-4 w-4" /> Existing repository
      </div>
      <div className="flex flex-col gap-2">
        <Field label="Clone URL">
          <div className="flex gap-2">
            <Input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://github.com/owner/repo.git" className="font-mono" />
            <Button onClick={clone} disabled={busy} variant="outline"><Download className="h-3.5 w-3.5" /></Button>
          </div>
        </Field>

        {dir && (
          <>
            <div className="text-xs text-secondary">
              Working dir: <code className="font-mono">{dir}</code>
              {changes.length > 0 && <span className="ml-2">· {changes.length} change(s)</span>}
            </div>
            {changes.length > 0 && (
              <div className="rounded-md border border-border bg-bg p-2 font-mono text-xs">
                {changes.map((c) => (
                  <div key={c.filepath}>
                    <span className={c.status === "deleted" ? "text-red" : c.status === "new" ? "text-green" : "text-amber"}>{c.status[0].toUpperCase()}</span>{" "}
                    {c.filepath}
                  </div>
                ))}
              </div>
            )}
            <Field label="Edit / add a file">
              <Input value={path} onChange={(e) => setPath(e.target.value)} placeholder="path/to/file.txt" className="mb-2 font-mono" />
              <textarea
                value={content}
                onChange={(e) => setContent(e.target.value)}
                rows={3}
                placeholder="file contents…"
                className="w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-xs focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
              />
              <Button onClick={stageWrite} disabled={busy} variant="outline" className="mt-2 self-start">Write file</Button>
            </Field>
            <Field label="Commit + push">
              <div className="flex gap-2">
                <Input value={message} onChange={(e) => setMessage(e.target.value)} placeholder="commit message" />
                <Button onClick={commitPush} disabled={busy}><Upload className="h-3.5 w-3.5" /></Button>
              </div>
            </Field>
            {commits.length > 0 && (
              <div className="mt-1 rounded-md border border-border bg-bg p-2 font-mono text-[11px] text-secondary">
                {commits.slice(0, 6).map((c) => (
                  <div key={c.oid} className="truncate"><span className="text-muted">{c.oid}</span> {c.message}</div>
                ))}
              </div>
            )}
          </>
        )}
      </div>
    </Card>
  );
}

/**
 * GitOps mirror (issue #4). Shows where dashboard CRUD ops are reflected to as
 * declarative config: a linked remote config repo, or — with zero setup — the
 * LOCAL in-browser provider, a real isomorphic-git repo that materializes the
 * OpenEdge artifact tree (openedge.yaml, per-project specs, meta, CI) and commits
 * on every change. Every git/CRUD op is also logged to the browser console.
 */
function GitOpsMirrorCard() {
  const [snap, setSnap] = useState<LocalSnapshot | null>(null);
  const [linked, setLinked] = useState(false);
  const [commits, setCommits] = useState<Array<{ oid: string; message: string; author: string; ts: number }>>([]);
  const [openFile, setOpenFile] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [team, setTeam] = useState("personal");

  const refresh = useCallback(async () => {
    setLinked(!localProviderActive());
    setTeam(currentTeam());
    setSnap(readLocalSnapshot());
    setCommits(await localGitLog());
  }, []);

  useEffect(() => {
    refresh();
    // The provider commits ~asynchronously after a CRUD mutation fires the event;
    // re-read the snapshot shortly after so the UI reflects the new commit.
    const onSync = () => window.setTimeout(refresh, 900);
    window.addEventListener("gitops-sync", onSync);
    return () => window.removeEventListener("gitops-sync", onSync);
  }, [refresh]);

  async function syncNow() {
    if (busy) return;
    setBusy(true);
    try {
      await syncLocalGitops();
      await refresh();
    } finally {
      setBusy(false);
    }
  }

  function downloadFile(path: string, content: string) {
    const blob = new Blob([content], { type: "text/plain" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = path.split("/").pop() || "artifact.yaml";
    a.click();
    URL.revokeObjectURL(a.href);
  }

  return (
    <Card className="mb-5 p-5">
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2 font-semibold">
          <FolderGit2 className="h-4 w-4" /> GitOps mirror
          {linked ? (
            <Badge tone="green"><Cloud className="mr-1 h-3 w-3" /> Remote repo</Badge>
          ) : (
            <Badge tone="blue"><HardDrive className="mr-1 h-3 w-3" /> Local provider</Badge>
          )}
        </div>
        {!linked && (
          <Button onClick={syncNow} disabled={busy} variant="outline" className="self-start">
            <RefreshCw className={`mr-1.5 h-3.5 w-3.5 ${busy ? "animate-spin" : ""}`} /> Sync now
          </Button>
        )}
      </div>

      {linked ? (
        <p className="text-xs text-secondary">
          A remote config repo is linked — every CRUD op (new project/deployment, deletions, settings,
          env, regions) is committed to it automatically. Open DevTools to see each reflection logged.
        </p>
      ) : (
        <p className="text-xs text-secondary">
          No remote config repo linked, so OpenEdge mirrors your config <strong>locally in this browser</strong> as
          a real git repo — translating every dashboard CRUD op into versioned GitOps artifacts. Connect GitHub
          from <span className="font-mono">Integrations</span> to push these to a repo instead.
        </p>
      )}

      {snap ? (
        <div className="mt-4 grid grid-cols-1 gap-4 lg:grid-cols-2">
          <div>
            <div className="mb-2 flex items-center gap-3 text-xs text-secondary">
              <span><code className="font-mono text-fg">{snap.repo}</code></span>
              <span>· {snap.projectCount} project{snap.projectCount === 1 ? "" : "s"}</span>
              <span>· {snap.files.length} artifacts</span>
              {snap.commit && <span>· <code className="font-mono">{snap.commit}</code></span>}
            </div>
            <div className="rounded-md border border-border bg-bg">
              {snap.files.map((f) => (
                <div key={f.path} className="border-b border-border last:border-0">
                  <button
                    onClick={() => setOpenFile(openFile === f.path ? null : f.path)}
                    className="flex w-full items-center justify-between px-3 py-1.5 text-left font-mono text-xs hover:bg-card"
                  >
                    <span className="flex items-center gap-1.5 truncate"><FileCode2 className="h-3.5 w-3.5 shrink-0 text-muted" /> {f.path}</span>
                    <span className="ml-2 shrink-0 text-muted">{f.content.length} B</span>
                  </button>
                  {openFile === f.path && (
                    <div className="border-t border-border bg-card p-2">
                      <div className="mb-1 flex justify-end">
                        <button onClick={() => downloadFile(f.path, f.content)} className="text-[11px] text-link hover:underline">
                          Download
                        </button>
                      </div>
                      <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all text-[11px] leading-relaxed text-secondary">{f.content}</pre>
                    </div>
                  )}
                </div>
              ))}
            </div>
          </div>
          <div>
            <div className="mb-2 text-xs font-medium text-secondary">Local commit history</div>
            <div className="rounded-md border border-border bg-bg p-2 font-mono text-[11px] text-secondary">
              {commits.length === 0 ? (
                <span className="text-muted">No commits yet — trigger a CRUD op or Sync now.</span>
              ) : (
                commits.slice(0, 12).map((c) => (
                  <div key={c.oid} className="truncate"><span className="text-muted">{c.oid}</span> {c.message}</div>
                ))
              )}
            </div>
            <p className="mt-2 text-[11px] text-muted">
              Last synced {snap.syncedAt ? new Date(snap.syncedAt).toLocaleString() : "—"} · team{" "}
              <code className="font-mono">{team}</code>
            </p>
          </div>
        </div>
      ) : (
        <p className="mt-4 text-xs text-muted">
          No local mirror yet. Make any config change (or click <strong>Sync now</strong>) to materialize the GitOps
          artifact tree in this browser.
        </p>
      )}
    </Card>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <label className="mb-1 block text-xs font-medium text-secondary">{label}</label>
      {children}
    </div>
  );
}
