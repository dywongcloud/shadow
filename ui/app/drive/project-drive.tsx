"use client";

import { useMemo, useRef, useState } from "react";
import { ChevronRight, Download, File, Folder, FolderPlus, HardDrive, Loader2, RefreshCw, Trash2, Upload } from "lucide-react";
import { Card, Button, Table, Th, Td } from "@/components/ui";
import { apiGetBytes, apiPutBytes, apiSend, usePoll } from "@/lib/api";
import { timeAgo, cn } from "@/lib/utils";
import { toast } from "@/components/toast";
import { WebdavPanel } from "./webdav-panel";

interface DriveNode {
  id: string;
  name: string;
  kind: "file" | "dir";
  size_bytes: number;
  mime?: string;
  content_hash?: string;
  mtime: number;
  ctime: number;
}
interface DriveListResp {
  path: string;
  entries: DriveNode[];
}

function formatBytes(n: number): string {
  if (!n) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${i === 0 ? v : v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** `path` join for the drive_api.rs convention — root is `""` (never `"/"`),
 *  segments join with a single `/`, no leading slash. */
function join(base: string, name: string): string {
  return base ? `${base}/${name}` : name;
}

/**
 * A real file browser against `/v1/drive/:project/*` — breadcrumb navigation,
 * folder/file listing, upload, create-folder, delete, plus the WebDAV mount
 * panel. `key={project}` on the caller remounts this component wholesale on
 * project switch, which is what resets `path` and every in-flight UI state
 * with no manual effect needed.
 */
export function ProjectDrive({ project }: { project: string }) {
  const [path, setPath] = useState("");
  const { data, error, refresh, loading } = usePoll<DriveListResp>(
    `/v1/drive/${encodeURIComponent(project)}/list?path=${encodeURIComponent(path)}`,
    6000
  );

  const entries = useMemo(() => {
    const list = data?.entries ?? [];
    return [...list].sort((a, b) =>
      a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "dir" ? -1 : 1
    );
  }, [data]);

  const [showMkdir, setShowMkdir] = useState(false);
  const [newFolder, setNewFolder] = useState("");
  const [busyMkdir, setBusyMkdir] = useState(false);
  const [uploading, setUploading] = useState(0);
  const [rowBusy, setRowBusy] = useState("");
  const fileInputRef = useRef<HTMLInputElement>(null);

  const crumbs = useMemo(() => {
    const segs = path ? path.split("/").filter(Boolean) : [];
    const out: { label: string; path: string }[] = [{ label: project, path: "" }];
    let acc = "";
    for (const s of segs) {
      acc = join(acc, s);
      out.push({ label: s, path: acc });
    }
    return out;
  }, [path, project]);

  function errText(e: unknown): string {
    return String(e instanceof Error ? e.message : e).replace(/^Error:\s*/, "");
  }

  async function createFolder() {
    const name = newFolder.trim();
    if (!name) return;
    setBusyMkdir(true);
    try {
      await apiSend("POST", `/v1/drive/${encodeURIComponent(project)}/mkdir`, { path: join(path, name) });
      setNewFolder("");
      setShowMkdir(false);
      refresh();
      toast(`Created "${name}"`, { tone: "blue" });
    } catch (e) {
      toast(`Couldn't create folder: ${errText(e)}`, {});
    } finally {
      setBusyMkdir(false);
    }
  }

  async function handleUpload(files: FileList | null) {
    if (!files || files.length === 0) return;
    const list = Array.from(files);
    setUploading(list.length);
    let ok = 0;
    for (const file of list) {
      try {
        await apiPutBytes(
          `/v1/drive/${encodeURIComponent(project)}/file?path=${encodeURIComponent(join(path, file.name))}`,
          file,
          file.type || "application/octet-stream"
        );
        ok++;
      } catch (e) {
        toast(`Couldn't upload "${file.name}": ${errText(e)}`, {});
      } finally {
        setUploading((n) => Math.max(0, n - 1));
      }
    }
    if (ok) toast(`Uploaded ${ok} file${ok === 1 ? "" : "s"}`, { tone: "blue" });
    refresh();
    if (fileInputRef.current) fileInputRef.current.value = "";
  }

  async function downloadFile(entry: DriveNode) {
    setRowBusy(entry.id);
    try {
      const blob = await apiGetBytes(
        `/v1/drive/${encodeURIComponent(project)}/file?path=${encodeURIComponent(join(path, entry.name))}`
      );
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = entry.name;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (e) {
      toast(`Couldn't download "${entry.name}": ${errText(e)}`, {});
    } finally {
      setRowBusy("");
    }
  }

  async function removeEntry(entry: DriveNode) {
    const kind = entry.kind === "dir" ? "folder" : "file";
    if (!confirm(`Delete ${kind} "${entry.name}"?${entry.kind === "dir" ? " It must be empty." : ""}`)) return;
    setRowBusy(entry.id);
    try {
      await apiSend("DELETE", `/v1/drive/${encodeURIComponent(project)}/file?path=${encodeURIComponent(join(path, entry.name))}`);
      refresh();
      toast(`Deleted "${entry.name}"`, { tone: "blue" });
    } catch (e) {
      toast(`Couldn't delete "${entry.name}": ${errText(e)}`, {});
    } finally {
      setRowBusy("");
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <Card className="flex flex-wrap items-center justify-between gap-3 p-4">
        <div className="flex min-w-0 flex-wrap items-center gap-0.5 text-sm">
          {crumbs.map((c, i, arr) => (
            <span key={c.path} className="flex items-center gap-0.5">
              <button
                type="button"
                onClick={() => setPath(c.path)}
                className={cn(
                  "inline-flex items-center gap-1 rounded px-1.5 py-0.5 hover:bg-subtle",
                  i === arr.length - 1 ? "font-medium text-fg" : "text-secondary hover:text-fg"
                )}
              >
                {i === 0 && <HardDrive className="h-3.5 w-3.5 shrink-0" />}
                {c.label}
              </button>
              {i < arr.length - 1 && <ChevronRight className="h-3.5 w-3.5 shrink-0 text-muted" />}
            </span>
          ))}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="outline" onClick={() => refresh()} title="Refresh">
            <RefreshCw className="h-4 w-4" />
          </Button>
          <Button variant="outline" onClick={() => setShowMkdir((v) => !v)}>
            <FolderPlus className="h-4 w-4" /> New folder
          </Button>
          <input ref={fileInputRef} type="file" multiple className="hidden" onChange={(e) => handleUpload(e.target.files)} />
          <Button onClick={() => fileInputRef.current?.click()} disabled={uploading > 0}>
            {uploading > 0 ? <Loader2 className="h-4 w-4 animate-spin" /> : <Upload className="h-4 w-4" />}
            {uploading > 0 ? `Uploading ${uploading}…` : "Upload"}
          </Button>
        </div>
      </Card>

      {showMkdir && (
        <Card className="flex flex-wrap items-end gap-2 p-4">
          <div className="flex-1">
            <label className="mb-1 block text-xs font-medium text-secondary">Folder name</label>
            <input
              value={newFolder}
              onChange={(e) => setNewFolder(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && createFolder()}
              placeholder="assets"
              autoFocus
              className="w-full max-w-xs rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none focus:ring-1 focus:ring-link"
            />
          </div>
          <Button onClick={createFolder} disabled={busyMkdir || !newFolder.trim()}>
            {busyMkdir ? <Loader2 className="h-4 w-4 animate-spin" /> : "Create"}
          </Button>
          <Button
            variant="ghost"
            onClick={() => {
              setShowMkdir(false);
              setNewFolder("");
            }}
          >
            Cancel
          </Button>
        </Card>
      )}

      {error && (
        <Card className="border-red-500/30 bg-red-500/5 text-sm text-red-500">
          Couldn&apos;t load this folder: {errText(error)}
        </Card>
      )}

      <Table>
        <thead>
          <tr>
            <Th>Name</Th>
            <Th>Size</Th>
            <Th>Modified</Th>
            <Th />
          </tr>
        </thead>
        <tbody>
          {entries.map((e) => (
            <tr key={e.id}>
              <Td className="font-medium">
                {e.kind === "dir" ? (
                  <button
                    type="button"
                    onClick={() => setPath(join(path, e.name))}
                    className="inline-flex items-center gap-2 hover:underline"
                  >
                    <Folder className="h-4 w-4 shrink-0 text-blue-500 dark:text-blue-400" /> {e.name}
                  </button>
                ) : (
                  <button
                    type="button"
                    onClick={() => downloadFile(e)}
                    disabled={rowBusy === e.id}
                    title={e.mime || "Download"}
                    className="inline-flex items-center gap-2 hover:underline disabled:opacity-50"
                  >
                    <File className="h-4 w-4 shrink-0 text-muted" /> {e.name}
                  </button>
                )}
              </Td>
              <Td className="text-secondary">{e.kind === "dir" ? "—" : formatBytes(e.size_bytes)}</Td>
              <Td className="text-muted">{timeAgo(e.mtime)}</Td>
              <Td>
                <div className="flex items-center justify-end gap-3">
                  {e.kind === "file" && (
                    <button
                      onClick={() => downloadFile(e)}
                      disabled={rowBusy === e.id}
                      title="Download"
                      className="text-muted hover:text-fg disabled:opacity-50"
                    >
                      {rowBusy === e.id ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Download className="h-3.5 w-3.5" />}
                    </button>
                  )}
                  <button
                    onClick={() => removeEntry(e)}
                    disabled={rowBusy === e.id}
                    title="Delete"
                    className="text-muted hover:text-red-500 disabled:opacity-50"
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </button>
                </div>
              </Td>
            </tr>
          ))}
          {!loading && entries.length === 0 && !error && <tr><Td className="text-muted">This folder is empty.</Td></tr>}
          {loading && !data && <tr><Td className="text-muted">Loading…</Td></tr>}
        </tbody>
      </Table>

      <WebdavPanel project={project} />
    </div>
  );
}
