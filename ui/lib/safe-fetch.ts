import "server-only";
import dns from "node:dns";
import type { IncomingHttpHeaders, IncomingMessage } from "node:http";
import { request as httpsRequest } from "node:https";
import type { LookupFunction } from "node:net";
import { Readable, Transform, pipeline } from "node:stream";
import { isPublicAddress, refusedHost } from "@/lib/public-address";

const MAX_REDIRECTS = 3;
const REDIRECT_STATUSES = new Set([301, 302, 303, 307, 308]);
const SOCKET_IDLE_TIMEOUT_MS = 10_000;

/** A failure that already knows the HTTP status a route should answer with. */
export class UpstreamError extends Error {
  constructor(
    readonly status: 400 | 403 | 502,
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

/** The whole body in memory, destroying the upstream the moment it exceeds `maxBytes`. */
export async function readBounded(source: IncomingMessage, maxBytes: number): Promise<Buffer> {
  const chunks: Buffer[] = [];
  let seen = 0;
  for await (const chunk of source as AsyncIterable<Buffer>) {
    seen += chunk.length;
    if (seen > maxBytes) {
      source.destroy();
      throw new UpstreamError(502, "upstream body exceeds the size cap");
    }
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

/** The body as a web stream that errors, and tears the upstream connection down, past `maxBytes` or when the consumer cancels. */
export function boundedBody(source: IncomingMessage, maxBytes: number): ReadableStream<Uint8Array> {
  let seen = 0;
  const counter = new Transform({
    transform(chunk: Buffer, _encoding, done) {
      seen += chunk.length;
      done(seen > maxBytes ? new UpstreamError(502, "upstream body exceeds the size cap") : null, chunk);
    },
  });
  pipeline(source, counter, () => {});
  return Readable.toWeb(counter) as unknown as ReadableStream<Uint8Array>;
}
