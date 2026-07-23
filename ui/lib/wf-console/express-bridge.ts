// lib/wf-console/express-bridge.ts
//
// Minimal Node<->Web bridge that lets an Express handler (IncomingMessage/
// ServerResponse based) serve a Next.js App Router request (Request ->
// Response). Used by app/wf-console/[[...slug]]/route.ts to mount the literal
// upstream @workflow/web Express app inside hive's dashboard — no iframe, no
// separate port, the upstream code answers each request in-process.
//
// Ported from the agentos wf-app bridge. Handles everything the WDK console
// actually does: synchronous writes, JSON/HTML payloads, static assets and —
// critically — BINARY CBOR bodies on /api/rpc (chunks must never go through
// String(), which mangles them and surfaces client-side as "end of buffer not
// reached" CBOR decode errors).

import { IncomingMessage, ServerResponse } from "node:http";
import { Socket } from "node:net";

type ExpressHandler = (
  req: IncomingMessage,
  res: ServerResponse
) => void | Promise<void>;

export async function expressToFetch(
  app: ExpressHandler,
  req: Request,
  rewriteUrl?: string
): Promise<Response> {
  const url = new URL(req.url);
  const targetUrl = rewriteUrl ?? url.pathname + url.search;

  // Drain the body once so it can be replayed as a Node readable stream.
  let bodyBuf: Buffer | null = null;
  if (req.body && req.method !== "GET" && req.method !== "HEAD") {
    const ab = await req.arrayBuffer();
    bodyBuf = Buffer.from(ab);
  }

  return new Promise<Response>((resolve, reject) => {
    // Fake socket — Express pokes at this for remote address etc.
    const socket = new Socket();

    const incoming = new IncomingMessage(socket);
    incoming.method = req.method;
    incoming.url = targetUrl;
    const headersObj: Record<string, string> = {};
    req.headers.forEach((v, k) => {
      headersObj[k.toLowerCase()] = v;
    });
    // Express 5's req.get()/req.hostname read from these.
    incoming.headers = headersObj;

    process.nextTick(() => {
      if (bodyBuf && bodyBuf.length > 0) incoming.push(bodyBuf);
      incoming.push(null);
    });

    const chunks: Buffer[] = [];
    let statusCode = 200;
    const respHeaders: Record<string, string | string[]> = {};
    let resolved = false;

    const res = new ServerResponse(incoming);

    const finalize = () => {
      if (resolved) return;
      resolved = true;
      const body = chunks.length ? Buffer.concat(chunks) : Buffer.alloc(0);
      const h = new Headers();
      for (const [name, val] of Object.entries(respHeaders)) {
        if (Array.isArray(val)) {
          for (const v of val) h.append(name, String(v));
        } else if (val !== undefined && val !== null) {
          h.set(name, String(val));
        }
      }
      // 204/304 must not carry a body per the Fetch spec.
      const bodyAllowed = statusCode !== 204 && statusCode !== 304;
      resolve(
        new Response(bodyAllowed ? new Uint8Array(body) : null, {
          status: statusCode,
          headers: h,
        })
      );
    };

    const origWriteHead = res.writeHead.bind(res);
    res.writeHead = ((
      code: number,
      reasonOrHeaders?: string | Record<string, string | string[]> | unknown,
      maybeHeaders?: Record<string, string | string[]>
    ) => {
      statusCode = code;
      const maybe =
        typeof reasonOrHeaders === "object" && reasonOrHeaders
          ? (reasonOrHeaders as Record<string, string | string[]>)
          : maybeHeaders;
      if (maybe) {
        for (const [k, v] of Object.entries(maybe)) {
          respHeaders[k.toLowerCase()] = v;
        }
      }
      return origWriteHead(code, reasonOrHeaders as never, maybeHeaders as never);
    }) as typeof res.writeHead;

    const origSetHeader = res.setHeader.bind(res);
    res.setHeader = ((name: string, value: number | string | readonly string[]) => {
      const v = Array.isArray(value) ? value.map(String) : String(value);
      respHeaders[name.toLowerCase()] = v;
      return origSetHeader(name, value as never);
    }) as typeof res.setHeader;

    const origRemoveHeader = res.removeHeader.bind(res);
    res.removeHeader = ((name: string) => {
      delete respHeaders[name.toLowerCase()];
      return origRemoveHeader(name);
    }) as typeof res.removeHeader;

    // Canonical chunk -> Buffer conversion. Must preserve binary payloads
    // (CBOR responses from /api/rpc are Uint8Arrays).
    const toBuf = (chunk: unknown, encoding?: unknown): Buffer => {
      if (Buffer.isBuffer(chunk)) return chunk as Buffer;
      if (chunk instanceof Uint8Array) return Buffer.from(chunk);
      if (chunk instanceof ArrayBuffer) return Buffer.from(chunk);
      if (typeof chunk === "string") {
        return Buffer.from(chunk, (encoding as BufferEncoding | undefined) ?? "utf8");
      }
      return Buffer.from(chunk as ArrayBufferLike);
    };

    res.write = ((chunk?: unknown, encoding?: unknown, cb?: unknown) => {
      if (chunk != null) chunks.push(toBuf(chunk, encoding));
      if (typeof encoding === "function") (encoding as (e?: Error | null) => void)();
      else if (typeof cb === "function") (cb as (e?: Error | null) => void)();
      return true;
    }) as typeof res.write;

    res.end = ((chunk?: unknown, encoding?: unknown, cb?: unknown) => {
      if (chunk != null && typeof chunk !== "function") {
        chunks.push(toBuf(chunk, encoding));
      }
      if (typeof chunk === "function") (chunk as () => void)();
      else if (typeof encoding === "function") (encoding as () => void)();
      else if (typeof cb === "function") (cb as () => void)();
      finalize();
      return res;
    }) as typeof res.end;

    Promise.resolve(app(incoming, res)).catch((e) => {
      if (!resolved) reject(e);
    });
  });
}
