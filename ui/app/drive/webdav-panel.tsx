"use client";

import { useState } from "react";
import { Check, Copy, Eye, EyeOff, KeyRound, Loader2, TriangleAlert } from "lucide-react";
import { Button, Card } from "@/components/ui";
import { apiSend } from "@/lib/api";
import { copyText } from "@/lib/utils";
import { toast } from "@/components/toast";

interface WebdavTokenResp {
  project: string;
  username: string;
  password: string;
  webdav_url: string;
}

/**
 * `/v1/drive/:project/webdav-token` mints (or rotates) the project's ONE
 * WebDAV Basic-auth credential — the backend stores only its SHA-256 hash
 * (`webdav_token_mint`'s own doc comment), so the plaintext password is
 * genuinely shown-once, same discipline as the API Keys page's created-token
 * reveal. Masked by default with a reveal toggle since it's a real
 * credential (the database-detail page's Reveal-secrets precedent).
 *
 * The backend hands back a FULLY QUALIFIED URL against its own public API
 * host (`CloudState::api_base()`), not a relative path — a real WebDAV
 * client (Finder/Explorer/rclone/davfs2) is a native OS network client with
 * no CORS restriction, so it must NOT go through the dashboard's same-origin
 * `/cloud` proxy (that proxy exists only to avoid CORS for the dashboard's
 * own browser-side fetches, and does not reliably pass through WebDAV's
 * non-standard methods like PROPFIND/MKCOL — confirmed live: PROPFIND
 * through `/cloud` times out, the same request against the direct API host
 * answers correctly).
 */
export function WebdavPanel({ project }: { project: string }) {
  const [cred, setCred] = useState<WebdavTokenResp | null>(null);
  const [revealed, setRevealed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState("");

  async function mint() {
    setBusy(true);
    try {
      const r = await apiSend<WebdavTokenResp>("POST", `/v1/drive/${encodeURIComponent(project)}/webdav-token`);
      setCred(r);
      setRevealed(false);
    } catch (e) {
      toast(`Couldn't mint WebDAV credentials: ${String(e instanceof Error ? e.message : e).replace(/^Error:\s*/, "")}`, {});
    } finally {
      setBusy(false);
    }
  }

  async function copy(key: string, value: string) {
    const ok = await copyText(value);
    if (ok) {
      setCopied(key);
      setTimeout(() => setCopied(""), 1200);
      toast(`Copied ${key}`, { tone: "blue" });
    } else {
      toast("Copy failed — select the value and copy manually", {});
    }
  }

  return (
    <Card>
      <div className="mb-3">
        <h3 className="text-sm font-semibold">Mount as a network drive</h3>
        <p className="mt-1 text-xs text-secondary">
          Standard WebDAV (RFC 4918) — mount with Finder&apos;s &quot;Connect to Server&quot;, Windows&apos; &quot;Map Network
          Drive&quot;, or a client like <code className="font-mono">rclone</code>/<code className="font-mono">davfs2</code>.
        </p>
      </div>
      <div className="flex flex-col gap-3">
        {cred && (
          <>
            <CopyField label="Server address" value={cred.webdav_url} copied={copied === "address"} onCopy={() => copy("address", cred.webdav_url)} />
            <CopyField label="Username" value={project} copied={copied === "username"} onCopy={() => copy("username", project)} />
          </>
        )}
        <div>
          <div className="mb-1 block text-xs font-medium text-secondary">Password</div>
          {cred ? (
            <div className="flex items-center gap-2">
              <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-subtle/50 px-3 py-2 font-mono text-xs">
                {revealed ? cred.password : "•".repeat(28)}
              </code>
              <Button variant="outline" onClick={() => setRevealed((v) => !v)} title={revealed ? "Hide" : "Reveal"}>
                {revealed ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
              </Button>
              <Button variant="outline" onClick={() => copy("password", cred.password)}>
                {copied === "password" ? <Check className="h-4 w-4 text-green" /> : <Copy className="h-4 w-4" />}
              </Button>
            </div>
          ) : (
            <span className="text-sm text-muted">No credential minted yet — mint one below to mount this drive.</span>
          )}
        </div>
        <div>
          <Button onClick={mint} disabled={busy}>
            {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <KeyRound className="h-4 w-4" />}
            {cred ? "Mint a new credential" : "Mint WebDAV credential"}
          </Button>
        </div>
        {cred && (
          <p className="flex items-start gap-1.5 text-xs text-amber-600 dark:text-amber-400">
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            This password is shown only once — copy it now. Minting a new credential immediately invalidates this one.
          </p>
        )}
      </div>
    </Card>
  );
}

function CopyField({ label, value, copied, onCopy }: { label: string; value: string; copied: boolean; onCopy: () => void }) {
  return (
    <div>
      <div className="mb-1 block text-xs font-medium text-secondary">{label}</div>
      <div className="flex items-center gap-2">
        <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-subtle/50 px-3 py-2 font-mono text-xs">{value}</code>
        <Button variant="outline" onClick={onCopy}>
          {copied ? <Check className="h-4 w-4 text-green" /> : <Copy className="h-4 w-4" />}
        </Button>
      </div>
    </div>
  );
}
