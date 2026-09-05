"use client";

import { useEffect, useRef, useState, useCallback } from "react";
import { Terminal as TerminalIcon, RotateCw, Maximize2, Minimize2 } from "lucide-react";
import { Card, Button, Badge } from "@/components/ui";
import { wsBase, currentTeam, mintSessionToken } from "@/lib/api";
import "@xterm/xterm/css/xterm.css";

type ConnState = "connecting" | "open" | "wrong-node" | "closed" | "error";

/**
 * Interactive sandbox terminal (Vercel-Sandbox-parity) — a real pty over
 * `GET .../sandboxes/:id/shell` (see `crates/hive-cloud/src/sandboxes_api.rs`):
 * raw byte frames both directions, so `vim`/`less`/`^C`/tab-completion all
 * work exactly as they would over SSH, unlike the Run Command panel above
 * (one-shot, line-buffered, no stdin after launch).
 *
 * xterm.js is loaded eagerly (no dynamic import) but only ever TOUCHES the
 * DOM/websocket inside `useEffect` — safe under SSR since this whole file is
 * a client component and the effect never runs server-side.
 *
 * A browser `WebSocket` hides the handshake: a refused upgrade (403 wrong
 * tenant, 404 deleted sandbox, an anonymous request because the session
 * cookie never reached the api host) surfaces only as `onerror` + `onclose`
 * with no status, and the console line "WebSocket connection … failed:" says
 * nothing (2026-09-02, `sbx_86001c0447a14171`). So a close BEFORE open runs
 * `diagnoseHandshake`: the same sandbox is read through the same-origin
 * `/cloud` proxy, which exercises the identical tenant + record gate minus
 * the upgrade and DOES return a status. 200 there means the record and the
 * caller are fine and only the direct api-host hop refused — the one thing
 * that differs is the credential the browser attached, i.e. the `hive_jwt`
 * cookie was not sent cross-subdomain — so the session is re-minted (which
 * sets the parent-domain cookie) and the connect is retried once. Anything
 * else is written into the pane verbatim instead of a silent "disconnected".
 */
export function TerminalPanel({ project, sandboxId }: { project: string; sandboxId: string }) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<import("@xterm/xterm").Terminal | null>(null);
  const fitRef = useRef<import("@xterm/addon-fit").FitAddon | null>(null);
  const socketRef = useRef<WebSocket | null>(null);
  const retriedRef = useRef(false);
  const [state, setState] = useState<ConnState>("closed");
  const [wrongNodeOwner, setWrongNodeOwner] = useState<string | null>(null);
  const [diagnosis, setDiagnosis] = useState<string | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [connectNonce, setConnectNonce] = useState(0);

  const retryConnect = useCallback(() => {
    socketRef.current?.close();
    termRef.current?.dispose();
    termRef.current = null;
    fitRef.current = null;
    setConnectNonce((n) => n + 1);
  }, []);

  const diagnoseHandshake = useCallback(
    async (term: import("@xterm/xterm").Terminal) => {
      let status = 0;
      let body = "";
      try {
        const r = await fetch(`/cloud/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandboxId}`, {
          headers: { "x-hive-team": currentTeam() },
          cache: "no-store",
        });
        status = r.status;
        body = (await r.text().catch(() => "")).slice(0, 200);
      } catch (e) {
        status = -1;
        body = String(e);
      }
      let text: string;
      if (status === 200) {
        if (!retriedRef.current) {
          retriedRef.current = true;
          term.writeln(
            "\r\n\x1b[33m[terminal] the api host refused the handshake although the sandbox is reachable — refreshing the session cookie and retrying…\x1b[0m",
          );
          const granted = await mintSessionToken();
          if (granted !== null) {
            retryConnect();
            return;
          }
          text = "Session refresh failed; sign out and back in, then reconnect.";
        } else {
          text =
            "The api host refused the WebSocket handshake although the sandbox is reachable through the dashboard. Reload the page (a fresh session cookie is issued on load), then reconnect.";
        }
      } else if (status === 404) {
        text = "This sandbox no longer exists (404) — it was deleted or its timeout expired.";
      } else if (status === 401 || status === 403) {
        text = `Not authorized for this sandbox under the current team (${status}): ${body || "no detail"}`;
      } else if (status === -1) {
        text = `The dashboard could not reach the API to check the sandbox: ${body}`;
      } else {
        text = `Handshake failed; the sandbox check answered HTTP ${status}: ${body || "no detail"}`;
      }
      setDiagnosis(text);
      setState("error");
      term.writeln(`\r\n\x1b[31m[terminal] ${text}\x1b[0m`);
    },
    [project, sandboxId, retryConnect],
  );

  const connect = useCallback(async () => {
    const el = containerRef.current;
    if (!el) return;

    // Lazy module import (not a lazy DOM attach) — keeps xterm's ~250KB out
    // of the initial sandbox-detail bundle since most visits never open the
    // terminal, without deferring the actual browser APIs this effect needs.
    const [{ Terminal }, { FitAddon }] = await Promise.all([
      import("@xterm/xterm"),
      import("@xterm/addon-fit"),
    ]);

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "var(--font-mono, ui-monospace, monospace)",
      theme: { background: "#00000000" },
      scrollback: 5000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);
    fit.fit();
    termRef.current = term;
    fitRef.current = fit;

    setState("connecting");
    setWrongNodeOwner(null);
    setDiagnosis(null);
    const url = `${wsBase()}/v1/projects/${encodeURIComponent(project)}/sandboxes/${sandboxId}/shell?cols=${term.cols}&rows=${term.rows}`;
    const socket = new WebSocket(url);
    socket.binaryType = "arraybuffer";
    socketRef.current = socket;

    let opened = false;
    let disposed = false;
    socket.onopen = () => {
      opened = true;
      retriedRef.current = false;
      setState("open");
    };
    socket.onclose = () => {
      setState((s) => (s === "wrong-node" ? s : "closed"));
      // Closed before it ever opened = the handshake itself was refused; the
      // browser gives no status, so go and get one.
      if (!opened && !disposed) void diagnoseHandshake(term);
    };
    socket.onerror = () => setState("error");
    socket.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        try {
          const msg = JSON.parse(ev.data);
          if (msg.type === "wrong_node") {
            setWrongNodeOwner(msg.owner ?? null);
            setState("wrong-node");
            term.writeln(`\r\n\x1b[33m[this sandbox is hosted on node "${msg.owner}" — reconnect from a session against that node]\x1b[0m`);
          } else if (msg.type === "exited") {
            term.writeln(`\r\n\x1b[2m[process exited${msg.exit_code != null ? ` (${msg.exit_code})` : ""}]\x1b[0m`);
            setState("closed");
          }
        } catch {
          term.write(ev.data);
        }
        return;
      }
      term.write(new Uint8Array(ev.data as ArrayBuffer));
    };

    term.onData((data) => {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(new TextEncoder().encode(data));
      }
    });

    return () => {
      disposed = true;
      socket.close();
      term.dispose();
    };
  }, [project, sandboxId, diagnoseHandshake]);

  useEffect(() => {
    let cleanup: (() => void) | undefined;
    connect().then((c) => {
      cleanup = c;
    });
    return () => {
      cleanup?.();
      termRef.current = null;
      fitRef.current = null;
      socketRef.current = null;
    };
  }, [connect, connectNonce]);

  // Resize the pty when the terminal element's box changes (container
  // resize, expand/collapse toggle, window resize) — ResizeObserver instead
  // of a window `resize` listener so a layout-only change (the expand
  // toggle) is also caught.
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      const term = termRef.current;
      const fit = fitRef.current;
      const socket = socketRef.current;
      if (!term || !fit) return;
      fit.fit();
      if (socket?.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [expanded]);

  function reconnect() {
    retriedRef.current = false;
    retryConnect();
  }

  const tone = state === "open" ? "green" : state === "connecting" ? "amber" : state === "wrong-node" ? "amber" : "red";
  const label =
    state === "open" ? "connected" : state === "connecting" ? "connecting…" : state === "wrong-node" ? "wrong node" : state === "error" ? "error" : "disconnected";

  return (
    <Card className={expanded ? "fixed inset-4 z-50 flex flex-col" : ""}>
      <div className="mb-2 flex items-center justify-between">
        <h3 className="flex items-center gap-1.5 text-sm font-semibold">
          <TerminalIcon className="h-4 w-4" /> Terminal
        </h3>
        <div className="flex items-center gap-2">
          <Badge tone={tone}>{label}</Badge>
          <Button variant="ghost" onClick={reconnect} title="Reconnect">
            <RotateCw className="h-3.5 w-3.5" />
          </Button>
          <Button variant="ghost" onClick={() => setExpanded((v) => !v)} title={expanded ? "Collapse" : "Expand"}>
            {expanded ? <Minimize2 className="h-3.5 w-3.5" /> : <Maximize2 className="h-3.5 w-3.5" />}
          </Button>
        </div>
      </div>
      {wrongNodeOwner ? (
        <p className="mb-2 text-xs text-amber-500">
          This sandbox&apos;s cell is hosted on node <code className="font-mono">{wrongNodeOwner}</code>. The terminal only
          connects to the owning node directly — try again from a session routed there.
        </p>
      ) : null}
      {diagnosis ? <p className="mb-2 text-xs text-red-500">{diagnosis}</p> : null}
      <div
        ref={containerRef}
        className={`overflow-hidden rounded-lg border border-border bg-black/90 p-2 ${expanded ? "flex-1" : "h-96"}`}
      />
    </Card>
  );
}
