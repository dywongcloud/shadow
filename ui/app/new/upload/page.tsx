"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import Link from "next/link";
import { ArrowLeft, Upload, Loader2, FileArchive, X, ChevronDown, Plus, KeyRound } from "lucide-react";
import { Card, Button, Input } from "@/components/ui";
import { currentTeam } from "@/lib/api";
import { TeamSelect } from "@/components/team-picker";
import { cn } from "@/lib/utils";

function slug(s: string) {
  return s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
}
const MAX_ZIP = 10 * 1024 * 1024;

/**
 * Dedicated "create project from a .zip" view — the no-Git alternative linked from
 * /new. Drag-drop or browse a source archive, name it, set env, Create. The archive
 * POSTs to /v1/deploy/zip (the `/cloud` rewrite streams it to the admin); on success
 * we jump straight to the build logs (no git "Preparing" animation, since there's no
 * repo to clone).
 */
export default function UploadProjectPage() {
  const router = useRouter();
  const [file, setFile] = useState<File | null>(null);
  const [name, setName] = useState("");
  const [dragOver, setDragOver] = useState(false);
  const [uploading, setUploading] = useState(false);
  const [error, setError] = useState("");
  const [team, setTeam] = useState<string>(() => currentTeam());
  const [envOpen, setEnvOpen] = useState(false);
  const [envRows, setEnvRows] = useState<{ k: string; v: string }[]>([{ k: "", v: "" }]);

  useEffect(() => {
    const sync = () => setTeam(currentTeam());
    window.addEventListener("hive-team-changed", sync);
    return () => window.removeEventListener("hive-team-changed", sync);
  }, []);

  function pick(f: File | undefined | null) {
    setError("");
    if (!f) return;
    if (!f.name.toLowerCase().endsWith(".zip")) {
      setError("Please choose a .zip file.");
      return;
    }
    if (f.size > MAX_ZIP) {
      setError("Zip too large (max 10 MB). Upload your source only — dependencies install during the build.");
      return;
    }
    setFile(f);
    if (!name) setName(slug(f.name.replace(/\.zip$/i, "")));
  }

  function env(): Record<string, string> {
    const out: Record<string, string> = {};
    for (const r of envRows) if (r.k.trim()) out[r.k.trim()] = r.v;
    return out;
  }
  function onEnvPaste(e: React.ClipboardEvent, idx: number) {
    const text = e.clipboardData.getData("text");
    if (!text.includes("\n") && !text.includes("=")) return;
    e.preventDefault();
    const parsed = text
      .split("\n").map((l) => l.trim())
      .filter((l) => l && !l.startsWith("#") && l.includes("="))
      .map((l) => { const i = l.indexOf("="); return { k: l.slice(0, i).trim().replace(/^export\s+/, ""), v: l.slice(i + 1).trim().replace(/^["']|["']$/g, "") }; });
    if (!parsed.length) return;
    setEnvRows((cur) => { const next = [...cur]; next.splice(idx, 1, ...parsed); return next.filter((r) => r.k || r.v).concat({ k: "", v: "" }); });
  }

  async function create() {
    if (!file) return;
    setError("");
    setUploading(true);
    try {
      const proj = name || slug(file.name.replace(/\.zip$/i, "")) || "project";
      // Scope the new project to the chosen team (matches the New Project flow).
      if (typeof window !== "undefined") {
        localStorage.setItem("hive_team", team === "personal" ? "__personal__" : team);
        window.dispatchEvent(new Event("hive-team-changed"));
      }
      const e = env();
      const meta = { project: proj, filename: file.name, env: Object.keys(e).length ? e : undefined, production: true };
      const metaB64 = btoa(unescape(encodeURIComponent(JSON.stringify(meta))));
      const res = await fetch("/cloud/v1/deploy/zip", {
        method: "POST",
        headers: { "content-type": "application/zip", "x-hive-team": team, "x-hive-deploy-meta": metaB64 },
        body: file,
      });
      if (!res.ok) throw new Error((await res.text().catch(() => "")) || `Upload failed (${res.status})`);
      const { build_id } = (await res.json()) as { build_id: string };
      router.push(`/deploy/${build_id}`);
    } catch (err) {
      setError(String(err));
      setUploading(false);
    }
  }

  const envCount = envRows.filter((r) => r.k.trim()).length;

  return (
    <div className="mx-auto max-w-2xl">
      <Link href="/new" className="mb-6 inline-flex items-center gap-2 text-sm text-secondary hover:text-fg">
        <ArrowLeft className="h-4 w-4" /> Back
      </Link>
      <Card className="p-6 sm:p-8">
        <h1 className="mb-1.5 text-2xl font-semibold tracking-tight">Upload a project</h1>
        <p className="mb-6 text-sm text-secondary">
          Deploy a <span className="font-mono">.zip</span> of your project — no Git required. We extract it, build it, and deploy it.
        </p>

        {!file ? (
          <div
            onDragOver={(e) => { e.preventDefault(); setDragOver(true); }}
            onDragLeave={() => setDragOver(false)}
            onDrop={(e) => { e.preventDefault(); setDragOver(false); pick(e.dataTransfer.files?.[0]); }}
            className={cn("rounded-xl border-2 border-dashed p-10 transition-colors", dragOver ? "border-fg bg-subtle/60" : "border-border")}
          >
            <input id="zip" type="file" accept=".zip,application/zip" className="hidden"
              onChange={(e) => { pick(e.target.files?.[0]); e.currentTarget.value = ""; }} />
            <label htmlFor="zip" className="flex cursor-pointer flex-col items-center gap-2 text-center">
              <Upload className="h-7 w-7 text-muted" />
              <span className="text-sm"><span className="font-medium text-fg">Drop your .zip here</span> or click to browse</span>
              <span className="text-xs text-muted">Source only, ≤ 10 MB. Dependencies install during the build.</span>
            </label>
          </div>
        ) : (
          <div className="flex items-center justify-between gap-3 rounded-xl border border-border bg-subtle/40 p-4">
            <div className="flex min-w-0 items-center gap-3">
              <FileArchive className="h-6 w-6 shrink-0 text-secondary" />
              <div className="min-w-0">
                <div className="truncate text-sm font-medium">{file.name}</div>
                <div className="text-xs text-muted">{(file.size / 1024).toFixed(0)} KB</div>
              </div>
            </div>
            <button onClick={() => setFile(null)} className="shrink-0 rounded-md p-2 text-muted hover:bg-subtle hover:text-fg" aria-label="Remove file">
              <X className="h-4 w-4" />
            </button>
          </div>
        )}

        <div className="mt-6">
          <label className="mb-1.5 block text-sm text-secondary">Project Name</label>
          <Input value={name} placeholder="auto-generated from the file name" onChange={(e) => setName(slug(e.target.value))} />
          <p className="mt-1 text-xs text-muted">Must be unique. Leave blank and we&apos;ll generate one.</p>
        </div>

        <div className="mt-5">
          <label className="mb-1.5 block text-sm text-secondary">Team</label>
          <TeamSelect value={team} onChange={setTeam} />
        </div>

        <div className="mt-5 rounded-md border border-border">
          <button type="button" onClick={() => setEnvOpen((o) => !o)} className="flex w-full items-center justify-between px-3 py-2.5 text-sm">
            <span className="flex items-center gap-2 font-medium">
              <KeyRound className="h-4 w-4 text-muted" /> Environment Variables
              {envCount > 0 && <span className="rounded-full bg-subtle px-2 py-0.5 text-xs text-secondary">{envCount}</span>}
            </span>
            <ChevronDown className={cn("h-4 w-4 text-muted transition-transform", envOpen ? "" : "-rotate-90")} />
          </button>
          {envOpen && (
            <div className="border-t border-border p-3">
              <div className="flex flex-col gap-2">
                {envRows.map((row, i) => (
                  <div key={i} className="flex items-center gap-2">
                    <Input className="flex-1 font-mono text-xs" placeholder="KEY" value={row.k}
                      onPaste={(e) => onEnvPaste(e, i)}
                      onChange={(e) => setEnvRows((c) => c.map((r, j) => (j === i ? { ...r, k: e.target.value } : r)))} />
                    <Input className="flex-1 font-mono text-xs" placeholder="value" value={row.v}
                      onChange={(e) => setEnvRows((c) => c.map((r, j) => (j === i ? { ...r, v: e.target.value } : r)))} />
                    <button type="button" className="shrink-0 rounded-md p-2 text-muted hover:bg-subtle hover:text-fg"
                      onClick={() => setEnvRows((c) => { const n = c.filter((_, j) => j !== i); return n.length ? n : [{ k: "", v: "" }]; })}>
                      <X className="h-4 w-4" />
                    </button>
                  </div>
                ))}
              </div>
              <button type="button" onClick={() => setEnvRows((c) => [...c, { k: "", v: "" }])}
                className="mt-3 inline-flex items-center gap-1.5 rounded-md border border-border px-2.5 py-1.5 text-xs text-secondary hover:bg-subtle">
                <Plus className="h-3.5 w-3.5" /> Add Variable
              </button>
              <p className="mt-2 text-xs text-muted">Tip: paste a .env into a KEY field to import many at once.</p>
            </div>
          )}
        </div>

        {error ? <p className="mt-4 text-sm text-red-600">{error}</p> : null}

        <Button onClick={create} disabled={!file || uploading} className="mt-6 w-full justify-center bg-fg py-2.5 text-bg">
          {uploading ? <><Loader2 className="h-4 w-4 animate-spin" /> Uploading…</> : "Create"}
        </Button>
      </Card>
    </div>
  );
}
