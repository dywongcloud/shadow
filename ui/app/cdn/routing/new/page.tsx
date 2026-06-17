"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import Link from "next/link";
import { ArrowLeft, Sparkles } from "lucide-react";
import { Button } from "@/components/ui";
import { apiSend } from "@/lib/api";

type Op = "Equals" | "Starts with" | "Matches";
type ActionKind = "" | "Redirect" | "Rewrite";

export default function NewRulePage() {
  const router = useRouter();
  const [nl, setNl] = useState("");
  const [op, setOp] = useState<Op>("Equals");
  const [path, setPath] = useState("");
  const [action, setAction] = useState<ActionKind>("");
  const [dest, setDest] = useState("");
  const [status, setStatus] = useState("308");
  const [name, setName] = useState("");
  const [desc, setDesc] = useState("");
  const [busy, setBusy] = useState(false);

  // Lightweight NL → rule generator: "redirect /old to /new with a 301".
  function generate() {
    const t = nl.toLowerCase();
    const m = t.match(/(redirect|rewrite)\s+(\S+)\s+to\s+(\S+)(?:.*?(\d{3}))?/);
    if (!m) { alert("Couldn't parse — try: redirect /old-blog/* to /blog/* with a 301"); return; }
    setAction(m[1] === "redirect" ? "Redirect" : "Rewrite");
    setOp(m[2].includes("*") ? "Starts with" : "Equals");
    setPath(m[2].replace(/\*$/, "").replace(/\/$/, "") || m[2]);
    setDest(m[3].replace(/\*$/, ""));
    if (m[4]) setStatus(m[4]);
    if (!name) setName(nl.slice(0, 40));
  }

  async function create() {
    if (!path || !action || !dest) { alert("Set a path, action, and destination."); return; }
    setBusy(true);
    try {
      const source = op === "Starts with" && !path.endsWith("/") ? `${path}/` : path;
      if (action === "Redirect") {
        await apiSend("POST", "/v1/routing/redirects", { source, destination: dest, status: Number(status) || 308 });
      } else {
        await apiSend("POST", "/v1/routing/rewrites", { source, destination: dest });
      }
      router.push("/cdn");
    } catch (e) { alert(String(e)); setBusy(false); }
  }

  const code = JSON.stringify(
    action === "Redirect"
      ? { redirects: [{ source: path, destination: dest, permanent: status === "308" || status === "301" }] }
      : { rewrites: [{ source: path, destination: dest }] },
    null, 2
  );

  return (
    <div>
      <Link href="/cdn" className="inline-flex items-center gap-1.5 text-sm text-link hover:underline"><ArrowLeft className="h-4 w-4" /> All Routes</Link>
      <h1 className="mb-8 mt-3 text-3xl font-semibold tracking-tight">New Rule</h1>

      {/* NL generation */}
      <div className="rounded-xl border border-border bg-card p-4">
        <textarea
          value={nl}
          onChange={(e) => setNl(e.target.value)}
          placeholder='Describe your routing rule, e.g. "redirect /old-blog/* to /blog/* with a 301"'
          className="h-28 w-full resize-none bg-transparent text-sm placeholder:text-muted focus:outline-none"
        />
        <div className="flex justify-end">
          <Button variant="outline" onClick={generate} disabled={!nl.trim()}><Sparkles className="h-4 w-4" /> Generate Rule</Button>
        </div>
      </div>

      <div className="my-6 flex items-center gap-3 text-xs text-muted">
        <span className="h-px flex-1 bg-border" /> OR <span className="h-px flex-1 bg-border" />
      </div>

      {/* Manual builder */}
      <div className="rounded-xl border border-border bg-card p-5">
        <div className="mb-3 text-sm font-semibold">Conditions</div>
        <div className="rounded-lg border border-border p-4">
          <div className="flex flex-wrap items-center gap-2 text-sm">
            <span className="text-secondary">If</span>
            <span className="font-medium">Request Path</span>
            <select value={op} onChange={(e) => setOp(e.target.value as Op)} className="rounded-md border border-border bg-card px-2 py-1.5 text-sm">
              <option>Equals</option><option>Starts with</option><option>Matches</option>
            </select>
            <input value={path} onChange={(e) => setPath(e.target.value)} placeholder="/api/users"
              className="flex-1 rounded-md border border-border bg-card px-3 py-1.5 font-mono text-sm focus:outline-none" />
          </div>
        </div>

        <div className="my-3 flex justify-center">
          <span className="rounded-md border border-border px-3 py-1 text-xs text-muted">+ AND</span>
        </div>

        <div className="rounded-lg border border-border p-4">
          <div className="flex flex-wrap items-center gap-2 text-sm">
            <span className="text-secondary">Then</span>
            <select value={action} onChange={(e) => setAction(e.target.value as ActionKind)} className="rounded-md border border-border bg-card px-2 py-1.5 text-sm">
              <option value="">Select action…</option>
              <option value="Redirect">Redirect to</option>
              <option value="Rewrite">Rewrite to</option>
            </select>
            {action && (
              <input value={dest} onChange={(e) => setDest(e.target.value)} placeholder="/destination"
                className="flex-1 rounded-md border border-border bg-card px-3 py-1.5 font-mono text-sm focus:outline-none" />
            )}
            {action === "Redirect" && (
              <select value={status} onChange={(e) => setStatus(e.target.value)} className="rounded-md border border-border bg-card px-2 py-1.5 text-sm">
                <option value="308">308 Permanent</option>
                <option value="307">307 Temporary</option>
                <option value="301">301 Moved</option>
                <option value="302">302 Found</option>
              </select>
            )}
          </div>
        </div>
      </div>

      <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-sm">Name</label>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="My routing rule…"
            className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none" />
        </div>
        <div>
          <label className="mb-1 block text-sm">Description (Optional)</label>
          <input value={desc} onChange={(e) => setDesc(e.target.value)} placeholder="Describe the purpose of this routing rule…"
            className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none" />
        </div>
      </div>

      <div className="mt-6 flex items-center justify-end gap-2">
        <Link href="/cdn"><Button variant="outline">Cancel</Button></Link>
        <Button variant="outline" onClick={() => alert(code)}>View Code</Button>
        <Button onClick={create} disabled={busy}>Create Rule</Button>
      </div>
    </div>
  );
}
