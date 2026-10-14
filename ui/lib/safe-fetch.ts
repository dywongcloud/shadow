import "server-only";
import dns from "node:dns";
import type { IncomingHttpHeaders, IncomingMessage } from "node:http";
import { request as httpsRequest } from "node:https";
import type { LookupFunction } from "node:net";
import { Readable, Transform, pipeline, type TransformCallback } from "node:stream";
import { createGunzip, createInflate, createInflateRaw } from "node:zlib";
import { isPublicAddress, refusedHost } from "@/lib/public-address";

const MAX_REDIRECTS = 3;
const REDIRECT_STATUSES = new Set([301, 302, 303, 307, 308]);
const SOCKET_IDLE_TIMEOUT_MS = 10_000;
const ENCODED_OVERHEAD_BYTES = 16 * 1024;
const DECODERS: Record<string, (firstByte: number) => Transform> = {
  gzip: () => createGunzip(),
  "x-gzip": () => createGunzip(),
  deflate: (firstByte) => ((firstByte & 0x0f) === 8 ? createInflate() : createInflateRaw()),
};

/** A failure that already knows the HTTP status a route should answer with. */
export class UpstreamError extends Error {
  constructor(
    readonly status: 400 | 403 | 415 | 502,
    message: string,
  ) {
    super(message);
  }
}

export interface SafeResponse {
  url: URL;
  status: number;
  headers: IncomingHttpHeaders;
  body: IncomingMessage;
}

const checkedLookup: LookupFunction = (hostname, options, callback) => {
  dns.lookup(hostname, { ...options, all: true }, (error, addresses) => {
    if (error) return callback(error, "", 0);
    if (addresses.some(({ address }) => !isPublicAddress(address))) {
      return callback(new UpstreamError(403, "target resolves to a non-public address") as unknown as NodeJS.ErrnoException, "", 0);
    }
    if (options.all) return callback(null, addresses);
    callback(null, addresses[0].address, addresses[0].family);
  });
};

function refusalFor(url: URL): UpstreamError | null {
  if (url.protocol !== "https:") return new UpstreamError(400, "only https upstreams are proxied");
  if (url.username || url.password) return new UpstreamError(400, "credentials in the url are not allowed");
  const reason = refusedHost(url.hostname);
  return reason ? new UpstreamError(403, reason) : null;
}

function requestOnce(url: URL, headers: Record<string, string>, signal: AbortSignal): Promise<IncomingMessage> {
  return new Promise((resolve, reject) => {
    const outgoing = httpsRequest(
      url,
      { method: "GET", headers, lookup: checkedLookup, agent: false, signal, timeout: SOCKET_IDLE_TIMEOUT_MS },
      resolve,
    );
    outgoing.once("timeout", () => outgoing.destroy(new Error("upstream timed out")));
    outgoing.on("error", reject);
    outgoing.end();
  });
}

/** GET a visitor-supplied https URL without letting it reach a private network: every address the name resolves to is validated inside the connect-time
 *  `lookup` (the check and the connection share one resolution), IP-literal hosts are validated up front, and redirects are followed by hand (at most 3) with
 *  every hop re-validated. `timeoutMs` bounds the whole exchange, response body included. */
export async function safeGet(
  url: URL,
  options: { headers: Record<string, string>; timeoutMs: number; signal?: AbortSignal },
): Promise<SafeResponse> {
  const signal = AbortSignal.any([AbortSignal.timeout(options.timeoutMs), ...(options.signal ? [options.signal] : [])]);
  let current = url;
  for (let hop = 0; ; hop++) {
    const refusal = refusalFor(current);
    if (refusal) throw refusal;
    const response = await requestOnce(current, options.headers, signal);
    const status = response.statusCode ?? 0;
    if (!REDIRECT_STATUSES.has(status)) return { url: current, status, headers: response.headers, body: response };
    response.destroy();
    const location = response.headers.location;
    if (!location) throw new UpstreamError(502, "redirect without a location");
    if (hop === MAX_REDIRECTS) throw new UpstreamError(502, `more than ${MAX_REDIRECTS} redirects`);
    current = new URL(location, current);
  }
}

/** The response's one content-coding: `""` for a plain body, its name when the proxy can undo it (gzip and deflate, zlib-wrapped or raw: 32 KiB windows), `null` for anything else (a chain, zstd, compress,
 *  brotli: its 16 MiB window is too much memory per request for an origin that ignored `accept-encoding: identity`). */
export function contentCoding(headers: IncomingHttpHeaders): string | null {
  const coding = String(headers["content-encoding"] ?? "").trim().toLowerCase();
  if (coding === "" || coding === "identity") return "";
  return Object.hasOwn(DECODERS, coding) ? coding : null;
}

function capped(limit: number): Transform {
  let seen = 0;
  return new Transform({
    transform(chunk: Buffer, _encoding, done) {
      seen += chunk.length;
      done(seen > limit ? new UpstreamError(502, "upstream body exceeds the size cap") : null, chunk);
    },
  });
}

/** A decoder made on the first non-empty chunk: a body with no bytes (a 204, `content-length: 0`) is an empty body, not an unexpected end of file, and `choose` can read the first byte
 *  (`deflate` is zlib-wrapped, yet some origins send it raw). The decoder is paused while this stream's readable side is full, so it never runs ahead of the reader. */
class LazyDecoder extends Transform {
  private decoder: Transform | null = null;

  constructor(private readonly choose: (firstByte: number) => Transform) {
    super();
  }

  override _transform(chunk: Buffer, _encoding: BufferEncoding, done: TransformCallback): void {
    if (chunk.length === 0) return done();
    if (!this.decoder) {
      const decoder = this.choose(chunk[0]);
      decoder.on("data", (decoded: Buffer) => {
        if (!this.push(decoded)) decoder.pause();
      });
      decoder.on("error", (error) => this.destroy(error));
      this.decoder = decoder;
    }
    this.decoder.write(chunk, done);
  }

  override _flush(done: TransformCallback): void {
    if (!this.decoder) return done();
    this.decoder.once("end", () => done());
    this.decoder.end();
  }

  override _read(size: number): void {
    this.decoder?.resume();
    super._read(size);
  }

  override _destroy(error: Error | null, callback: (error?: Error | null) => void): void {
    this.decoder?.destroy();
    super._destroy(error, callback);
  }
}

/** The origin's bytes as the caller meant them, at most `maxBytes` of them, the upstream torn down when the stream ends for any reason. A gzip/deflate body (an origin that ignored
 *  `accept-encoding: identity`) is decoded with the cap on BOTH sides of the decoder: the decoded side stops a bomb, the encoded side stops an endless stream that decodes to nothing. */
function boundedSource(response: SafeResponse, maxBytes: number): Readable {
  const coding = contentCoding(response.headers);
  if (coding === null || (coding !== "" && response.status === 206)) {
    response.body.destroy();
    throw coding === null
      ? new UpstreamError(415, "unsupported upstream content-encoding")
      : new UpstreamError(502, "a partial upstream response in a content-coding cannot be decoded");
  }
  const decoded = capped(maxBytes);
  const decoding = coding === "" ? [] : [capped(maxBytes + ENCODED_OVERHEAD_BYTES), new LazyDecoder(DECODERS[coding])];
  pipeline([response.body, ...decoding, decoded], () => {});
  return decoded;
}

/** The whole body in memory (decoded if the origin encoded it), the upstream destroyed the moment it exceeds `maxBytes`. */
export async function readBounded(response: SafeResponse, maxBytes: number): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of boundedSource(response, maxBytes)) chunks.push(chunk as Buffer);
  return Buffer.concat(chunks);
}

/** The body as a web stream (decoded if the origin encoded it) that errors, and tears the upstream connection down, past `maxBytes` or when the consumer cancels. */
export function boundedBody(response: SafeResponse, maxBytes: number): ReadableStream<Uint8Array> {
  return Readable.toWeb(boundedSource(response, maxBytes)) as unknown as ReadableStream<Uint8Array>;
}
