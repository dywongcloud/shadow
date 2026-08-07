// The Node API surface a browser-executed function runs against
// (browser-node-api-runtime).
//
// WHAT THIS IS. Every browser artifact is ONE `async function (request, ops)`
// expression (`browser_artifacts::bundle`) evaluated by
// pkg/function-worker.js — in the production donor lane inside QuickJS, whose
// global object is bare ECMAScript and NOTHING else. Measured against the
// shipped bundle, the guest has no `console`, no `TextEncoder`/`TextDecoder`,
// no `URL`, no `atob`/`btoa`, no timers, no `performance`, no `crypto`, no
// `fetch` — the build's old rejection message ("use Uint8Array/TextEncoder,
// both exist in QuickJS") named an API that is not there. So the substrate has
// to SUPPLY the Node surface, not merely stop rejecting the tokens for it:
// `wrapArtifactSource` prepends this runtime to the verified artifact source,
// giving the guest `require`, `process`, `Buffer`, `console`, timers,
// `URL`/`URLSearchParams`, `TextEncoder`/`TextDecoder`, the Node builtin
// module set, and the `(req, res)` calling convention every Node handler and
// every Express app is written against.
//
// WHY NOT almostnode. The obvious candidate (macaly/almostnode, MIT) is a
// browser-hosted Node EMULATOR, not a shim library: 16 MB unpacked / 230 KB
// gzipped, it owns module resolution, a virtual FS, an npm client and dev
// servers; it executes modules with host-realm `eval`, ASSIGNS
// `globalThis.process`, and its built bundle references `window` (175x),
// `document` (46x), `navigator`, `localStorage`, `Worker` and
// `navigator.serviceWorker` — none of which exist in a QuickJS guest, and its
// http/esbuild shims reach out to `fetch()` and third-party CDNs at runtime.
// Its own README rates `net`/`tls`/`dns`/`dgram`/`cluster`/`vm`/`v8` as
// "stubs only" and says untrusted code must run in a separately-deployed
// cross-origin iframe. Vendoring it here would ship a megabyte of DOM-coupled
// code that cannot boot in the substrate we actually execute in, into other
// people's browsers. What it genuinely provides for a sandboxed guest — a
// CommonJS registry over the Node builtins, `process`, `Buffer`, and an
// in-memory HTTP server bridge — is what this file implements directly,
// against this substrate's real primitives and its numbered host-op boundary.
//
// HONESTY RULE. A module the substrate cannot implement is NOT a silent no-op:
// `net`/`tls`/`dns`/`dgram`/`cluster`/`child_process`/`worker_threads`, and
// outbound `http.request`, throw a NAMED error at the point of use. That is the
// same discipline the removed build-time scan had — refuse loudly — moved from
// build time to the exact call that cannot work, so a handler that never takes
// that branch is no longer blocked from deploying at all.
//
// TRUST. This file adds NO capability: it runs INSIDE the guest, below the
// admission/capability boundary, and reaches the host only through `ops.call`,
// which is already bounded by the artifact's `allowed_ops`. It cannot see the
// donor's session, seed or endpoint, and `process.env` is deliberately EMPTY —
// project env and secrets never ship to a donor's browser.
//
// PUBLISHED ASSET: ui/scripts/sync-browser-node.mjs copies this file into
// ui/public/browser-node/. worker-function-runtime.js imports it STATICALLY, so
// omitting it there breaks the whole SharedWorker module graph on the fleet
// while every local check stays green.

// Each part is stringified with Function.prototype.toString and evaluated in
// the guest (the node-compat.js `guestMain.toString()` precedent), so the parts
// must close over NOTHING outside their two parameters: `g` (the guest global)
// and `R` (the shared runtime record the parts hand each other).

function installPrimitives(g, R) {
  // UTF-8 and base64 done by hand: the guest may have neither TextEncoder nor
  // atob/btoa, and every other part (Buffer, the request/response bridge, the
  // hex/base64 encodings) is built on these two pairs.
  R.utf8Encode = function (input) {
    const str = String(input);
    const out = [];
    for (let i = 0; i < str.length; i++) {
      let code = str.charCodeAt(i);
      if (code >= 0xd800 && code <= 0xdbff && i + 1 < str.length) {
        const next = str.charCodeAt(i + 1);
        if (next >= 0xdc00 && next <= 0xdfff) {
          code = 0x10000 + ((code - 0xd800) << 10) + (next - 0xdc00);
          i++;
        }
      }
      if (code < 0x80) out.push(code);
      else if (code < 0x800) out.push(0xc0 | (code >> 6), 0x80 | (code & 63));
      else if (code < 0x10000) out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
      else out.push(0xf0 | (code >> 18), 0x80 | ((code >> 12) & 63), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
    }
    return new Uint8Array(out);
  };
  R.utf8Decode = function (bytes) {
    const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes || 0);
    let out = "";
    let i = 0;
    while (i < view.length) {
      const a = view[i++];
      let code;
      if (a < 0x80) code = a;
      else if (a >= 0xc0 && a < 0xe0) code = ((a & 31) << 6) | (view[i++] & 63);
      else if (a >= 0xe0 && a < 0xf0) code = ((a & 15) << 12) | ((view[i++] & 63) << 6) | (view[i++] & 63);
      else if (a >= 0xf0) code = ((a & 7) << 18) | ((view[i++] & 63) << 12) | ((view[i++] & 63) << 6) | (view[i++] & 63);
      else code = 0xfffd;
      if (code > 0xffff) {
        code -= 0x10000;
        out += String.fromCharCode(0xd800 + (code >> 10), 0xdc00 + (code & 1023));
      } else {
        out += String.fromCharCode(code);
      }
    }
    return out;
  };
  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  R.base64Encode = function (bytes) {
    const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes || 0);
    let out = "";
    for (let i = 0; i < view.length; i += 3) {
      const a = view[i];
      const b = view[i + 1] || 0;
      const c = view[i + 2] || 0;
      out += B64[a >>> 2];
      out += B64[((a & 3) << 4) | (b >>> 4)];
      out += i + 1 < view.length ? B64[((b & 15) << 2) | (c >>> 6)] : "=";
      out += i + 2 < view.length ? B64[c & 63] : "=";
    }
    return out;
  };
  R.base64Decode = function (text) {
    let clean = String(text).replace(/[\r\n\t ]/g, "").split("-").join("+").split("_").join("/");
    while (clean.length % 4 !== 0) clean += "=";
    const padding = clean.endsWith("==") ? 2 : clean.endsWith("=") ? 1 : 0;
    const out = new Uint8Array(Math.max(0, (clean.length / 4) * 3 - padding));
    let written = 0;
    for (let i = 0; i < clean.length; i += 4) {
      const values = [0, 1, 2, 3].map(k => (clean[i + k] === "=" ? 0 : B64.indexOf(clean[i + k])));
      if (values.some(v => v < 0)) throw new TypeError("invalid base64 input");
      const bits = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
      if (written < out.length) out[written++] = (bits >>> 16) & 255;
      if (written < out.length) out[written++] = (bits >>> 8) & 255;
      if (written < out.length) out[written++] = bits & 255;
    }
    return out;
  };

  // Every global below is installed ONLY when the substrate lacks it: the
  // native-mode lane runs inside a real Worker that already has working
  // (and better) implementations, and replacing those would be a regression.
  if (typeof g.TextEncoder !== "function") {
    g.TextEncoder = class TextEncoder {
      get encoding() { return "utf-8"; }
      encode(input = "") { return R.utf8Encode(input); }
      encodeInto(input, dest) {
        const bytes = R.utf8Encode(input);
        const written = Math.min(bytes.length, dest.length);
        dest.set(bytes.subarray(0, written));
        return { read: input.length, written };
      }
    };
  }
  if (typeof g.TextDecoder !== "function") {
    g.TextDecoder = class TextDecoder {
      constructor(encoding = "utf-8") {
        const label = String(encoding).toLowerCase();
        if (label !== "utf-8" && label !== "utf8" && label !== "unicode-1-1-utf-8") {
          throw new RangeError(`TextDecoder encoding ${JSON.stringify(encoding)} is unsupported in the browser function runtime (utf-8 only)`);
        }
        this.encoding = "utf-8";
      }
      decode(bytes) { return bytes === undefined ? "" : R.utf8Decode(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes.buffer || bytes)); }
    };
  }
  if (typeof g.btoa !== "function") {
    g.btoa = str => {
      const s = String(str);
      const bytes = new Uint8Array(s.length);
      for (let i = 0; i < s.length; i++) {
        const code = s.charCodeAt(i);
        if (code > 255) throw new TypeError("btoa input must be latin1");
        bytes[i] = code;
      }
      return R.base64Encode(bytes);
    };
  }
  if (typeof g.atob !== "function") {
    g.atob = text => {
      const bytes = R.base64Decode(text);
      let out = "";
      for (let i = 0; i < bytes.length; i++) out += String.fromCharCode(bytes[i]);
      return out;
    };
  }

  // Console: the guest has no host log channel (a log op would be a WRITE
  // effect, and the canonical policy encoding hardcodes effect=read), so lines
  // go to a bounded ring the host can read post-mortem via `process.__hiveLogs`
  // rather than to a black hole that also swallows the shape of a crash.
  R.logs = [];
  R.log = function (level, args) {
    let line = "";
    for (const arg of args) {
      if (line) line += " ";
      if (typeof arg === "string") line += arg;
      else {
        try { line += JSON.stringify(arg); } catch { line += String(arg); }
      }
    }
    R.logs.push({ level, line: line.slice(0, 4096) });
    if (R.logs.length > 256) R.logs.shift();
  };
  if (typeof g.console !== "object" || g.console === null) {
    const level = name => (...args) => R.log(name, args);
    g.console = {
      log: level("log"), info: level("info"), warn: level("warn"), error: level("error"),
      debug: level("debug"), trace: level("trace"), dir: level("log"),
      group: level("log"), groupEnd: () => {}, table: level("log"),
      time: () => {}, timeEnd: () => {}, assert: (ok, ...args) => { if (!ok) R.log("error", args); },
      count: () => {},
    };
  }

  // Timers. QuickJS has no clock-driven event loop: the only thing that can
  // run after the current job is a microtask, and the engine's interrupt
  // handler is what bounds it. So a timer callback runs when the job queue
  // drains, ordered by (delay, insertion) — the delay ORDERS timers, it does
  // not delay them. Firing early is the honest failure direction: the
  // alternative is a handler that hangs until the invocation deadline.
  if (typeof g.setTimeout !== "function") {
    R.timers = new Map();
    R.timerSeq = 1;
    R.timerFlush = false;
    const flush = () => {
      R.timerFlush = false;
      const due = [...R.timers.values()].sort((a, b) => a.delay - b.delay || a.id - b.id);
      for (const timer of due) {
        if (!R.timers.has(timer.id)) continue;
        if (!timer.repeat) R.timers.delete(timer.id);
        try { timer.fn(...timer.args); } catch (error) { R.log("error", ["uncaught timer error:", String(error && error.message || error)]); }
      }
      if (R.timers.size > 0) schedule();
    };
    const schedule = () => {
      if (R.timerFlush) return;
      R.timerFlush = true;
      Promise.resolve().then(flush);
    };
    const add = (fn, delay, args, repeat) => {
      if (typeof fn !== "function") throw new TypeError("callback must be a function");
      const id = R.timerSeq++;
      R.timers.set(id, { id, fn, args, delay: Number(delay) || 0, repeat });
      schedule();
      return id;
    };
    g.setTimeout = (fn, delay, ...args) => add(fn, delay, args, false);
    g.setInterval = (fn, delay, ...args) => add(fn, delay, args, true);
    g.setImmediate = (fn, ...args) => add(fn, 0, args, false);
    g.clearTimeout = id => { R.timers.delete(id); };
    g.clearInterval = g.clearTimeout;
    g.clearImmediate = g.clearTimeout;
  }
  if (typeof g.queueMicrotask !== "function") {
    g.queueMicrotask = fn => { Promise.resolve().then(fn); };
  }
  if (typeof g.performance !== "object" || g.performance === null) {
    const origin = Date.now();
    g.performance = { now: () => Date.now() - origin, timeOrigin: origin };
  }
  if (typeof g.structuredClone !== "function") {
    const clone = value => {
      if (value === null || typeof value !== "object") return value;
      if (value instanceof Date) return new Date(value.getTime());
      if (value instanceof Uint8Array) return value.slice();
      if (Array.isArray(value)) return value.map(clone);
      if (value instanceof Map) return new Map([...value].map(([k, v]) => [clone(k), clone(v)]));
      if (value instanceof Set) return new Set([...value].map(clone));
      const out = {};
      for (const key of Object.keys(value)) out[key] = clone(value[key]);
      return out;
    };
    g.structuredClone = clone;
  }
  if (typeof g.AbortController !== "function") {
    class AbortSignal {
      constructor() { this.aborted = false; this.reason = undefined; this._listeners = []; }
      addEventListener(type, fn) { if (type === "abort") this._listeners.push(fn); }
      removeEventListener(type, fn) { this._listeners = this._listeners.filter(x => x !== fn); }
      throwIfAborted() { if (this.aborted) throw this.reason; }
    }
    g.AbortSignal = AbortSignal;
    g.AbortController = class AbortController {
      constructor() { this.signal = new AbortSignal(); }
      abort(reason) {
        if (this.signal.aborted) return;
        this.signal.aborted = true;
        this.signal.reason = reason === undefined ? new Error("This operation was aborted") : reason;
        for (const fn of this.signal._listeners.splice(0)) {
          try { fn({ type: "abort", target: this.signal }); } catch { /* listener errors never abort the abort */ }
        }
      }
    };
  }
}

function installUrl(g, R) {
  // WHATWG URL/URLSearchParams. Present in the native lane, absent in QuickJS —
  // and load-bearing there: the request bridge, `querystring`, the `url`
  // module and Express routing all parse through it.
  class URLSearchParams {
    constructor(init) {
      this._pairs = [];
      if (typeof init === "string") {
        for (const part of init.replace(/^\?/, "").split("&")) {
          if (!part) continue;
          const eq = part.indexOf("=");
          const key = eq < 0 ? part : part.slice(0, eq);
          const value = eq < 0 ? "" : part.slice(eq + 1);
          this._pairs.push([decodeURIComponent(key.split("+").join(" ")), decodeURIComponent(value.split("+").join(" "))]);
        }
      } else if (init instanceof URLSearchParams) {
        this._pairs = init._pairs.map(pair => [pair[0], pair[1]]);
      } else if (Array.isArray(init)) {
        this._pairs = init.map(pair => [String(pair[0]), String(pair[1])]);
      } else if (init && typeof init === "object") {
        this._pairs = Object.entries(init).map(([k, v]) => [String(k), String(v)]);
      }
      this._onchange = null;
    }
    _changed() { if (this._onchange) this._onchange(); }
    append(k, v) { this._pairs.push([String(k), String(v)]); this._changed(); }
    delete(k) { this._pairs = this._pairs.filter(pair => pair[0] !== String(k)); this._changed(); }
    get(k) { const hit = this._pairs.find(pair => pair[0] === String(k)); return hit ? hit[1] : null; }
    getAll(k) { return this._pairs.filter(pair => pair[0] === String(k)).map(pair => pair[1]); }
    has(k) { return this._pairs.some(pair => pair[0] === String(k)); }
    set(k, v) {
      const key = String(k);
      const at = this._pairs.findIndex(pair => pair[0] === key);
      if (at < 0) this._pairs.push([key, String(v)]);
      else {
        this._pairs[at] = [key, String(v)];
        this._pairs = this._pairs.filter((pair, i) => i <= at || pair[0] !== key);
      }
      this._changed();
    }
    sort() { this._pairs.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)); this._changed(); }
    forEach(fn, self) { for (const [k, v] of this._pairs) fn.call(self, v, k, this); }
    keys() { return this._pairs.map(pair => pair[0])[Symbol.iterator](); }
    values() { return this._pairs.map(pair => pair[1])[Symbol.iterator](); }
    entries() { return this._pairs.map(pair => [pair[0], pair[1]])[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    get size() { return this._pairs.length; }
    toString() {
      const esc = value => encodeURIComponent(value).split("%20").join("+");
      return this._pairs.map(([k, v]) => `${esc(k)}=${esc(v)}`).join("&");
    }
  }

  const ABSOLUTE = /^([A-Za-z][A-Za-z0-9+.-]*):\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/;
  const OPAQUE = /^([A-Za-z][A-Za-z0-9+.-]*):([^/].*)$/;

  class URL {
    constructor(input, base) {
      let href = String(input);
      if (!ABSOLUTE.test(href) && !OPAQUE.test(href)) {
        if (base === undefined) throw new TypeError(`Invalid URL: ${href}`);
        const parent = base instanceof URL ? base : new URL(String(base));
        if (href.startsWith("//")) href = `${parent.protocol}${href}`;
        else if (href.startsWith("/")) href = `${parent.protocol}//${parent.host}${href}`;
        else if (href.startsWith("?")) href = `${parent.protocol}//${parent.host}${parent.pathname}${href}`;
        else if (href.startsWith("#")) href = `${parent.protocol}//${parent.host}${parent.pathname}${parent.search}${href}`;
        else {
          const dir = parent.pathname.slice(0, parent.pathname.lastIndexOf("/") + 1);
          href = `${parent.protocol}//${parent.host}${dir}${href}`;
        }
      }
      const match = ABSOLUTE.exec(href);
      if (!match) {
        const opaque = OPAQUE.exec(href);
        if (!opaque) throw new TypeError(`Invalid URL: ${href}`);
        this.protocol = `${opaque[1]}:`;
        this.username = ""; this.password = ""; this.hostname = ""; this.port = "";
        this.pathname = opaque[2]; this.hash = "";
        this._search = "";
      } else {
        this.protocol = `${match[1]}:`;
        let authority = match[2];
        const at = authority.lastIndexOf("@");
        this.username = ""; this.password = "";
        if (at >= 0) {
          const credentials = authority.slice(0, at);
          authority = authority.slice(at + 1);
          const colon = credentials.indexOf(":");
          this.username = colon < 0 ? credentials : credentials.slice(0, colon);
          this.password = colon < 0 ? "" : credentials.slice(colon + 1);
        }
        const portAt = authority.lastIndexOf(":");
        if (portAt > authority.lastIndexOf("]")) {
          this.hostname = authority.slice(0, portAt).toLowerCase();
          this.port = authority.slice(portAt + 1);
        } else {
          this.hostname = authority.toLowerCase();
          this.port = "";
        }
        // Resolve `.`/`..` the way the WHATWG parser does — a browser artifact
        // is not a filesystem, but code that builds paths still expects it.
        const raw = match[3] || "/";
        const segments = [];
        for (const part of raw.split("/")) {
          if (part === "." || part === "") continue;
          if (part === "..") segments.pop();
          else segments.push(part);
        }
        this.pathname = `/${segments.join("/")}${raw.endsWith("/") && segments.length ? "/" : ""}`;
        this._search = match[4] || "";
        this.hash = match[5] || "";
      }
      this.searchParams = new URLSearchParams(this._search);
      this.searchParams._onchange = () => {
        const text = this.searchParams.toString();
        this._search = text ? `?${text}` : "";
      };
    }
    get search() { return this._search; }
    set search(value) {
      const text = String(value).replace(/^\?/, "");
      this._search = text ? `?${text}` : "";
      const next = new URLSearchParams(this._search);
      this.searchParams._pairs = next._pairs;
    }
    get host() { return this.port ? `${this.hostname}:${this.port}` : this.hostname; }
    get origin() { return `${this.protocol}//${this.host}`; }
    get href() {
      const credentials = this.username ? `${this.username}${this.password ? `:${this.password}` : ""}@` : "";
      return `${this.protocol}//${credentials}${this.host}${this.pathname}${this.search}${this.hash}`;
    }
    toString() { return this.href; }
    toJSON() { return this.href; }
  }

  R.URL = typeof g.URL === "function" ? g.URL : URL;
  R.URLSearchParams = typeof g.URLSearchParams === "function" ? g.URLSearchParams : URLSearchParams;
  if (typeof g.URL !== "function") g.URL = URL;
  if (typeof g.URLSearchParams !== "function") g.URLSearchParams = URLSearchParams;
}

function installBuffer(g, R) {
  // A REAL Buffer: a Uint8Array subclass, so it passes `instanceof Uint8Array`,
  // `ArrayBuffer.isView`, and every byte-oriented API in the guest unchanged.
  const ENCODINGS = new Set(["utf8", "utf-8", "hex", "base64", "base64url", "latin1", "binary", "ascii", "ucs2", "ucs-2", "utf16le", "utf-16le"]);
  const normalizeEncoding = enc => {
    const name = String(enc || "utf8").toLowerCase();
    if (!ENCODINGS.has(name)) throw new TypeError(`Unknown encoding: ${enc}`);
    return name;
  };

  class Buffer extends Uint8Array {
    static from(value, encodingOrOffset, length) {
      if (typeof value === "string") {
        const encoding = normalizeEncoding(encodingOrOffset);
        if (encoding === "hex") {
          const clean = value.length % 2 ? value.slice(0, -1) : value;
          const out = new Buffer(clean.length / 2);
          for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(clean.slice(i * 2, i * 2 + 2), 16);
          return out;
        }
        if (encoding === "base64" || encoding === "base64url") return new Buffer(R.base64Decode(value));
        if (encoding === "latin1" || encoding === "binary" || encoding === "ascii") {
          const out = new Buffer(value.length);
          for (let i = 0; i < value.length; i++) out[i] = value.charCodeAt(i) & 255;
          return out;
        }
        if (encoding === "ucs2" || encoding === "ucs-2" || encoding === "utf16le" || encoding === "utf-16le") {
          const out = new Buffer(value.length * 2);
          for (let i = 0; i < value.length; i++) {
            const code = value.charCodeAt(i);
            out[i * 2] = code & 255;
            out[i * 2 + 1] = code >>> 8;
          }
          return out;
        }
        return new Buffer(R.utf8Encode(value));
      }
      if (value instanceof ArrayBuffer) {
        const offset = Number(encodingOrOffset) || 0;
        const size = length === undefined ? value.byteLength - offset : Number(length);
        const out = new Buffer(size);
        out.set(new Uint8Array(value, offset, size));
        return out;
      }
      if (ArrayBuffer.isView(value)) {
        const out = new Buffer(value.byteLength);
        out.set(new Uint8Array(value.buffer, value.byteOffset, value.byteLength));
        return out;
      }
      if (Array.isArray(value)) return new Buffer(value.map(n => Number(n) & 255));
      if (value && typeof value === "object" && value.type === "Buffer" && Array.isArray(value.data)) {
        return new Buffer(value.data.map(n => Number(n) & 255));
      }
      throw new TypeError("Buffer.from expects a string, array, ArrayBuffer or view");
    }
    static alloc(size, fill = 0, encoding) {
      const out = new Buffer(Number(size) || 0);
      if (typeof fill === "string" && fill.length) {
        const pattern = Buffer.from(fill, encoding);
        for (let i = 0; i < out.length; i++) out[i] = pattern[i % pattern.length];
      } else if (fill) {
        out.fill(Number(fill) & 255);
      }
      return out;
    }
    static allocUnsafe(size) { return Buffer.alloc(size); }
    static allocUnsafeSlow(size) { return Buffer.alloc(size); }
    static isBuffer(value) { return value instanceof Buffer; }
    static isEncoding(value) { return ENCODINGS.has(String(value).toLowerCase()); }
    static byteLength(value, encoding) {
      if (typeof value !== "string") return value ? value.byteLength ?? value.length ?? 0 : 0;
      return Buffer.from(value, encoding).length;
    }
    static concat(list, total) {
      const parts = [...list].map(item => (item instanceof Buffer ? item : Buffer.from(item)));
      const size = total === undefined ? parts.reduce((sum, part) => sum + part.length, 0) : Number(total);
      const out = Buffer.alloc(size);
      let offset = 0;
      for (const part of parts) {
        if (offset >= size) break;
        out.set(part.subarray(0, size - offset), offset);
        offset += part.length;
      }
      return out;
    }
    static compare(a, b) {
      const left = a instanceof Buffer ? a : Buffer.from(a);
      const right = b instanceof Buffer ? b : Buffer.from(b);
      const len = Math.min(left.length, right.length);
      for (let i = 0; i < len; i++) {
        if (left[i] !== right[i]) return left[i] < right[i] ? -1 : 1;
      }
      return left.length === right.length ? 0 : left.length < right.length ? -1 : 1;
    }
    toString(encoding, start = 0, end = this.length) {
      const name = normalizeEncoding(encoding);
      const view = this.subarray(Number(start) || 0, end === undefined ? this.length : Number(end));
      if (name === "hex") {
        let out = "";
        for (let i = 0; i < view.length; i++) out += view[i].toString(16).padStart(2, "0");
        return out;
      }
      if (name === "base64") return R.base64Encode(view);
      if (name === "base64url") return R.base64Encode(view).split("+").join("-").split("/").join("_").replace(/=+$/, "");
      if (name === "latin1" || name === "binary" || name === "ascii") {
        let out = "";
        for (let i = 0; i < view.length; i++) out += String.fromCharCode(name === "ascii" ? view[i] & 127 : view[i]);
        return out;
      }
      if (name === "ucs2" || name === "ucs-2" || name === "utf16le" || name === "utf-16le") {
        let out = "";
        for (let i = 0; i + 1 < view.length; i += 2) out += String.fromCharCode(view[i] | (view[i + 1] << 8));
        return out;
      }
      return R.utf8Decode(view);
    }
    toJSON() { return { type: "Buffer", data: [...this] }; }
    equals(other) { return Buffer.compare(this, other) === 0; }
    compare(other) { return Buffer.compare(this, other); }
    // Node's `slice` is a VIEW, not Uint8Array's copy. `subarray` is inherited:
    // TypedArray species construction already yields a Buffer for a subclass.
    slice(start, end) { return this.subarray(start, end); }
    write(string, offset = 0, length, encoding) {
      if (typeof offset === "string") { encoding = offset; offset = 0; length = undefined; }
      if (typeof length === "string") { encoding = length; length = undefined; }
      const bytes = Buffer.from(String(string), encoding);
      const count = Math.min(length === undefined ? bytes.length : Number(length), this.length - offset);
      this.set(bytes.subarray(0, count), offset);
      return count;
    }
    copy(target, targetStart = 0, sourceStart = 0, sourceEnd = this.length) {
      const slice = this.subarray(sourceStart, sourceEnd);
      const count = Math.min(slice.length, target.length - targetStart);
      target.set(slice.subarray(0, count), targetStart);
      return count;
    }
    readUInt8(offset = 0) { return this[offset]; }
    writeUInt8(value, offset = 0) { this[offset] = Number(value) & 255; return offset + 1; }
    readInt8(offset = 0) { const v = this[offset]; return v > 127 ? v - 256 : v; }
    writeInt8(value, offset = 0) { this[offset] = Number(value) & 255; return offset + 1; }
    readUInt16BE(offset = 0) { return (this[offset] << 8) | this[offset + 1]; }
    readUInt16LE(offset = 0) { return this[offset] | (this[offset + 1] << 8); }
    writeUInt16BE(value, offset = 0) { const v = Number(value) & 0xffff; this[offset] = v >>> 8; this[offset + 1] = v & 255; return offset + 2; }
    writeUInt16LE(value, offset = 0) { const v = Number(value) & 0xffff; this[offset] = v & 255; this[offset + 1] = v >>> 8; return offset + 2; }
    readUInt32BE(offset = 0) { return ((this[offset] << 24) >>> 0) + (this[offset + 1] << 16) + (this[offset + 2] << 8) + this[offset + 3]; }
    readUInt32LE(offset = 0) { return this[offset] + (this[offset + 1] << 8) + (this[offset + 2] << 16) + ((this[offset + 3] << 24) >>> 0); }
    writeUInt32BE(value, offset = 0) { const v = Number(value) >>> 0; this[offset] = (v >>> 24) & 255; this[offset + 1] = (v >>> 16) & 255; this[offset + 2] = (v >>> 8) & 255; this[offset + 3] = v & 255; return offset + 4; }
    writeUInt32LE(value, offset = 0) { const v = Number(value) >>> 0; this[offset] = v & 255; this[offset + 1] = (v >>> 8) & 255; this[offset + 2] = (v >>> 16) & 255; this[offset + 3] = (v >>> 24) & 255; return offset + 4; }
    readInt32BE(offset = 0) { return this.readUInt32BE(offset) | 0; }
    readInt32LE(offset = 0) { return this.readUInt32LE(offset) | 0; }
    writeInt32BE(value, offset = 0) { return this.writeUInt32BE(value, offset); }
    writeInt32LE(value, offset = 0) { return this.writeUInt32LE(value, offset); }
  }
  R.Buffer = Buffer;
  g.Buffer = Buffer;
}

function installModules(g, R) {
  // The CommonJS registry. Each entry is a LAZY factory: a handler that only
  // requires `path` never pays for `stream`, and a module that cannot exist
  // here throws when it is required, naming itself.
  const factories = new Map();
  const cache = new Map();
  R.defineModule = (name, factory) => { factories.set(name, factory); };
  R.moduleFactories = factories;
  R.moduleCache = cache;

  const unsupported = (name, detail) => {
    R.defineModule(name, () => new Proxy({}, {
      get(_target, prop) {
        if (prop === "__esModule" || typeof prop === "symbol") return undefined;
        return () => {
          throw new Error(`node:${name}.${String(prop)}() is unavailable in a browser function: ${detail}`);
        };
      },
    }));
  };

  // ---- path -------------------------------------------------------------
  const pathModule = {};
  pathModule.sep = "/";
  pathModule.delimiter = ":";
  pathModule.isAbsolute = value => String(value).startsWith("/");
  pathModule.normalize = value => {
    const raw = String(value);
    const absolute = raw.startsWith("/");
    const out = [];
    for (const part of raw.split("/")) {
      if (!part || part === ".") continue;
      if (part === "..") {
        if (out.length && out[out.length - 1] !== "..") out.pop();
        else if (!absolute) out.push("..");
      } else out.push(part);
    }
    const joined = out.join("/");
    if (absolute) return `/${joined}`;
    return joined || ".";
  };
  pathModule.join = (...parts) => {
    const joined = parts.filter(part => part !== "" && part !== undefined && part !== null).join("/");
    return joined ? pathModule.normalize(joined) : ".";
  };
  pathModule.resolve = (...parts) => {
    let resolved = "";
    for (let i = parts.length - 1; i >= 0; i--) {
      const part = String(parts[i] ?? "");
      if (!part) continue;
      resolved = resolved ? `${part}/${resolved}` : part;
      if (part.startsWith("/")) break;
    }
    if (!resolved.startsWith("/")) resolved = `/${resolved}`;
    return pathModule.normalize(resolved) || "/";
  };
  pathModule.dirname = value => {
    const norm = String(value).split("\\").join("/");
    const at = norm.lastIndexOf("/");
    if (at < 0) return ".";
    if (at === 0) return "/";
    return norm.slice(0, at);
  };
  pathModule.basename = (value, ext) => {
    const base = String(value).split("\\").join("/").split("/").filter(Boolean).pop() || "";
    return ext && base.endsWith(ext) ? base.slice(0, -ext.length) : base;
  };
  pathModule.extname = value => {
    const base = pathModule.basename(value);
    const at = base.lastIndexOf(".");
    return at <= 0 ? "" : base.slice(at);
  };
  pathModule.relative = (from, to) => {
    const a = pathModule.resolve(from).split("/").filter(Boolean);
    const b = pathModule.resolve(to).split("/").filter(Boolean);
    let shared = 0;
    while (shared < a.length && shared < b.length && a[shared] === b[shared]) shared++;
    return [...Array(a.length - shared).fill(".."), ...b.slice(shared)].join("/");
  };
  pathModule.parse = value => {
    const dir = pathModule.dirname(value);
    const base = pathModule.basename(value);
    const ext = pathModule.extname(value);
    return { root: String(value).startsWith("/") ? "/" : "", dir, base, ext, name: ext ? base.slice(0, -ext.length) : base };
  };
  pathModule.format = parts => {
    const base = parts.base || `${parts.name || ""}${parts.ext || ""}`;
    if (!parts.dir) return base;
    return parts.dir === "/" ? `/${base}` : `${parts.dir}/${base}`;
  };
  pathModule.toNamespacedPath = value => value;
  pathModule.posix = pathModule;
  pathModule.win32 = pathModule;
  R.path = pathModule;
  R.defineModule("path", () => pathModule);
  R.defineModule("path/posix", () => pathModule);
  R.defineModule("path/win32", () => pathModule);

  // ---- events -----------------------------------------------------------
  class EventEmitter {
    constructor() { this._events = new Map(); this._maxListeners = 10; }
    _list(type) {
      if (!this._events.has(type)) this._events.set(type, []);
      return this._events.get(type);
    }
    on(type, fn) { this._list(type).push(fn); return this; }
    addListener(type, fn) { return this.on(type, fn); }
    prependListener(type, fn) { this._list(type).unshift(fn); return this; }
    once(type, fn) {
      const wrapper = (...args) => { this.off(type, wrapper); fn.apply(this, args); };
      wrapper.listener = fn;
      return this.on(type, wrapper);
    }
    prependOnceListener(type, fn) {
      const wrapper = (...args) => { this.off(type, wrapper); fn.apply(this, args); };
      wrapper.listener = fn;
      return this.prependListener(type, wrapper);
    }
    off(type, fn) {
      this._events.set(type, this._list(type).filter(item => item !== fn && item.listener !== fn));
      return this;
    }
    removeListener(type, fn) { return this.off(type, fn); }
    removeAllListeners(type) {
      if (type === undefined) this._events.clear();
      else this._events.delete(type);
      return this;
    }
    emit(type, ...args) {
      const list = [...this._list(type)];
      if (type === "error" && list.length === 0) {
        throw args[0] instanceof Error ? args[0] : new Error(`Unhandled error. (${String(args[0])})`);
      }
      for (const fn of list) fn.apply(this, args);
      return list.length > 0;
    }
    listeners(type) { return [...this._list(type)]; }
    rawListeners(type) { return [...this._list(type)]; }
    listenerCount(type) { return this._list(type).length; }
    eventNames() { return [...this._events.keys()]; }
    setMaxListeners(n) { this._maxListeners = n; return this; }
    getMaxListeners() { return this._maxListeners; }
  }
  R.EventEmitter = EventEmitter;
  R.defineModule("events", () => {
    const module = EventEmitter;
    module.EventEmitter = EventEmitter;
    module.default = EventEmitter;
    module.once = (emitter, type) => new Promise((resolve, reject) => {
      emitter.once(type, (...args) => resolve(args));
      if (type !== "error" && typeof emitter.once === "function") emitter.once("error", reject);
    });
    module.setMaxListeners = () => {};
    return module;
  });

  // ---- util -------------------------------------------------------------
  const inspect = (value, depth = 2, seen = new Set()) => {
    if (typeof value === "string") return depth === 2 ? value : JSON.stringify(value);
    if (value === null || value === undefined || typeof value === "number" || typeof value === "boolean") return String(value);
    if (typeof value === "bigint") return `${value}n`;
    if (typeof value === "function") return `[Function: ${value.name || "anonymous"}]`;
    if (typeof value === "symbol") return String(value);
    if (value instanceof Error) return `${value.name}: ${value.message}`;
    if (value instanceof Date) return value.toISOString();
    if (value instanceof RegExp) return String(value);
    if (seen.has(value)) return "[Circular]";
    if (depth < 0) return Array.isArray(value) ? "[Array]" : "[Object]";
    seen.add(value);
    try {
      if (R.Buffer.isBuffer(value)) return `<Buffer ${[...value.subarray(0, 32)].map(b => b.toString(16).padStart(2, "0")).join(" ")}${value.length > 32 ? " ..." : ""}>`;
      if (Array.isArray(value)) return `[ ${value.map(item => inspect(item, depth - 1, seen)).join(", ")} ]`;
      if (value instanceof Map) return `Map(${value.size}) { ${[...value].map(([k, v]) => `${inspect(k, depth - 1, seen)} => ${inspect(v, depth - 1, seen)}`).join(", ")} }`;
      if (value instanceof Set) return `Set(${value.size}) { ${[...value].map(item => inspect(item, depth - 1, seen)).join(", ")} }`;
      const body = Object.keys(value).map(key => `${key}: ${inspect(value[key], depth - 1, seen)}`).join(", ");
      return body ? `{ ${body} }` : "{}";
    } finally {
      seen.delete(value);
    }
  };
  const format = (...args) => {
    if (typeof args[0] !== "string") return args.map(item => inspect(item, 1)).join(" ");
    const rest = args.slice(1);
    let index = 0;
    let out = args[0].replace(/%[sdifjoO%]/g, token => {
      if (token === "%%") return "%";
      if (index >= rest.length) return token;
      const value = rest[index++];
      if (token === "%s") return typeof value === "string" ? value : inspect(value, 1);
      if (token === "%d" || token === "%f") return String(Number(value));
      if (token === "%i") return String(Number.parseInt(value, 10));
      if (token === "%j") { try { return JSON.stringify(value); } catch { return "[Circular]"; } }
      return inspect(value, 1);
    });
    for (; index < rest.length; index++) out += ` ${typeof rest[index] === "string" ? rest[index] : inspect(rest[index], 1)}`;
    return out;
  };
  R.format = format;
  R.defineModule("util", () => ({
    inspect: Object.assign((value, options) => inspect(value, (options && options.depth) ?? 2), { custom: Symbol.for("nodejs.util.inspect.custom") }),
    format,
    formatWithOptions: (_options, ...args) => format(...args),
    promisify(fn) {
      return (...args) => new Promise((resolve, reject) => {
        fn(...args, (error, value) => (error ? reject(error) : resolve(value)));
      });
    },
    callbackify(fn) {
      return (...args) => {
        const callback = args.pop();
        Promise.resolve(fn(...args)).then(value => callback(null, value), error => callback(error));
      };
    },
    inherits(child, parent) {
      Object.setPrototypeOf(child.prototype, parent.prototype);
      Object.setPrototypeOf(child, parent);
    },
    deprecate(fn) { return fn; },
    isDeepStrictEqual(a, b) { return JSON.stringify(a) === JSON.stringify(b); },
    types: {
      isDate: value => value instanceof Date,
      isRegExp: value => value instanceof RegExp,
      isPromise: value => !!value && typeof value.then === "function",
      isMap: value => value instanceof Map,
      isSet: value => value instanceof Set,
      isTypedArray: value => ArrayBuffer.isView(value),
      isUint8Array: value => value instanceof Uint8Array,
      isArrayBuffer: value => value instanceof ArrayBuffer,
    },
    TextEncoder: g.TextEncoder,
    TextDecoder: g.TextDecoder,
    debuglog: () => () => {},
    _extend: (target, source) => Object.assign(target, source),
  }));

  // ---- stream / string_decoder -----------------------------------------
  R.defineModule("string_decoder", () => ({
    StringDecoder: class StringDecoder {
      constructor(encoding = "utf8") { this.encoding = encoding; }
      write(bytes) { return R.Buffer.from(bytes).toString(this.encoding); }
      end(bytes) { return bytes ? this.write(bytes) : ""; }
    },
  }));
  R.defineModule("stream", () => {
    // In-memory only: there is no socket and no file descriptor behind any of
    // these, so they are exactly as useful as a buffer with events — which is
    // what body assembly and simple transforms actually need.
    class Readable extends EventEmitter {
      constructor(options = {}) {
        super();
        this._chunks = [];
        this._ended = false;
        this._flowing = false;
        this.readable = true;
        if (typeof options.read === "function") this._read = options.read;
      }
      push(chunk) {
        if (chunk === null) { this._ended = true; this.emit("end"); return false; }
        this._chunks.push(chunk);
        if (this._flowing) this.emit("data", this._chunks.shift());
        return true;
      }
      read() { return this._chunks.shift() ?? null; }
      setEncoding(encoding) { this._encoding = encoding; return this; }
      resume() { this._flowing = true; while (this._chunks.length) this.emit("data", this._chunks.shift()); if (this._ended) this.emit("end"); return this; }
      pause() { this._flowing = false; return this; }
      on(type, fn) { super.on(type, fn); if (type === "data") this.resume(); return this; }
      pipe(destination) {
        this.on("data", chunk => destination.write(chunk));
        this.on("end", () => destination.end());
        return destination;
      }
      async *[Symbol.asyncIterator]() {
        while (this._chunks.length) yield this._chunks.shift();
      }
      static from(iterable) {
        const stream = new Readable();
        for (const item of iterable) stream.push(item);
        stream.push(null);
        return stream;
      }
    }
    class Writable extends EventEmitter {
      constructor(options = {}) {
        super();
        this._written = [];
        this.writable = true;
        if (typeof options.write === "function") this._write = options.write;
      }
      write(chunk, encoding, callback) {
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        if (this._write) this._write(chunk, encoding, callback || (() => {}));
        else { this._written.push(chunk); if (callback) callback(); }
        return true;
      }
      end(chunk, encoding, callback) {
        if (typeof chunk === "function") { callback = chunk; chunk = undefined; }
        if (chunk !== undefined) this.write(chunk, encoding);
        this.writable = false;
        this.emit("finish");
        if (callback) callback();
        return this;
      }
    }
    class PassThrough extends Readable {
      write(chunk) { this.push(chunk); return true; }
      end(chunk) { if (chunk !== undefined) this.push(chunk); this.push(null); return this; }
    }
    const module = { Readable, Writable, Duplex: PassThrough, Transform: PassThrough, PassThrough, Stream: Readable };
    module.pipeline = (...args) => {
      const callback = typeof args[args.length - 1] === "function" ? args.pop() : () => {};
      let current = args[0];
      for (const next of args.slice(1)) current = current.pipe(next);
      callback(null);
      return current;
    };
    module.finished = (stream, callback) => { stream.on("end", () => callback(null)); stream.on("finish", () => callback(null)); };
    module.promises = { pipeline: async (...args) => module.pipeline(...args) };
    return module;
  });

  // ---- querystring / url ------------------------------------------------
  R.defineModule("querystring", () => ({
    parse(text) {
      const params = new R.URLSearchParams(String(text || "").replace(/^\?/, ""));
      const out = {};
      for (const [key, value] of params.entries()) {
        if (key in out) out[key] = Array.isArray(out[key]) ? [...out[key], value] : [out[key], value];
        else out[key] = value;
      }
      return out;
    },
    stringify(value) {
      const params = new R.URLSearchParams();
      for (const [key, item] of Object.entries(value || {})) {
        if (Array.isArray(item)) for (const entry of item) params.append(key, entry);
        else params.append(key, item);
      }
      return params.toString();
    },
    escape: encodeURIComponent,
    unescape: decodeURIComponent,
  }));
  R.defineModule("url", () => {
    const querystring = R.require("querystring");
    return {
      URL: R.URL,
      URLSearchParams: R.URLSearchParams,
      parse(input, parseQuery) {
        const url = new R.URL(String(input), "http://localhost");
        return {
          protocol: url.protocol, host: url.host, hostname: url.hostname, port: url.port,
          pathname: url.pathname, search: url.search, hash: url.hash,
          path: `${url.pathname}${url.search}`, href: url.href,
          query: parseQuery ? querystring.parse(url.search.slice(1)) : url.search.slice(1),
        };
      },
      format(value) { return value instanceof R.URL ? value.href : String(value.href || ""); },
      resolve(from, to) { return new R.URL(to, from).href; },
      fileURLToPath(value) { return new R.URL(String(value)).pathname; },
      pathToFileURL(value) { return new R.URL(`file://${value}`); },
    };
  });

  // ---- os / assert / timers / perf_hooks --------------------------------
  R.defineModule("os", () => ({
    EOL: "\n",
    platform: () => "browser",
    type: () => "Browser",
    arch: () => "wasm",
    release: () => "1.0.0",
    hostname: () => "hive-browser-node",
    tmpdir: () => "/tmp",
    homedir: () => "/",
    cpus: () => [],
    totalmem: () => 0,
    freemem: () => 0,
    uptime: () => Math.floor((Date.now() - R.bootMs) / 1000),
    endianness: () => "LE",
    networkInterfaces: () => ({}),
    userInfo: () => ({ username: "browser", homedir: "/", shell: null }),
  }));
  R.defineModule("assert", () => {
    class AssertionError extends Error {
      constructor(options) {
        super(options.message || `${JSON.stringify(options.actual)} ${options.operator} ${JSON.stringify(options.expected)}`);
        this.name = "AssertionError";
        this.actual = options.actual;
        this.expected = options.expected;
        this.operator = options.operator;
      }
    }
    const fail = (actual, expected, message, operator) => { throw new AssertionError({ actual, expected, message, operator }); };
    const ok = (value, message) => { if (!value) fail(value, true, message, "=="); };
    const deep = (a, b) => JSON.stringify(a) === JSON.stringify(b);
    const assert = Object.assign(ok, {
      ok,
      AssertionError,
      equal: (a, b, m) => { if (a != b) fail(a, b, m, "=="); },
      notEqual: (a, b, m) => { if (a == b) fail(a, b, m, "!="); },
      strictEqual: (a, b, m) => { if (a !== b) fail(a, b, m, "==="); },
      notStrictEqual: (a, b, m) => { if (a === b) fail(a, b, m, "!=="); },
      deepEqual: (a, b, m) => { if (!deep(a, b)) fail(a, b, m, "deepEqual"); },
      deepStrictEqual: (a, b, m) => { if (!deep(a, b)) fail(a, b, m, "deepStrictEqual"); },
      notDeepStrictEqual: (a, b, m) => { if (deep(a, b)) fail(a, b, m, "notDeepStrictEqual"); },
      throws: (fn, _expected, m) => {
        try { fn(); } catch { return; }
        fail(undefined, undefined, m || "Missing expected exception", "throws");
      },
      doesNotThrow: fn => { fn(); },
      fail: m => fail(undefined, undefined, m || "Failed", "fail"),
    });
    assert.strict = assert;
    return assert;
  });
  R.defineModule("timers", () => ({
    setTimeout: (...args) => g.setTimeout(...args),
    clearTimeout: (...args) => g.clearTimeout(...args),
    setInterval: (...args) => g.setInterval(...args),
    clearInterval: (...args) => g.clearInterval(...args),
    setImmediate: (...args) => g.setImmediate(...args),
    clearImmediate: (...args) => g.clearImmediate(...args),
  }));
  R.defineModule("timers/promises", () => ({
    setTimeout: (delay, value) => new Promise(resolve => g.setTimeout(() => resolve(value), delay)),
    setImmediate: value => new Promise(resolve => g.setImmediate(() => resolve(value))),
  }));
  R.defineModule("perf_hooks", () => ({ performance: g.performance, PerformanceObserver: class PerformanceObserver { observe() {} disconnect() {} } }));

  // ---- crypto -----------------------------------------------------------
  R.defineModule("crypto", () => {
    // Pure-JS SHA-256 (+ HMAC): deterministic, needs no host capability, and
    // covers the overwhelmingly common hashing/signing-check use. Everything
    // that needs ENTROPY throws instead of inventing it — Math.random is not a
    // CSPRNG and a token minted from it would be a real vulnerability wearing
    // a `crypto` name.
    const K = [
      0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
      0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
      0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
      0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
      0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
      0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
      0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
      0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    const sha256 = bytes => {
      const h = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
      const length = bytes.length;
      const padded = new Uint8Array((((length + 8) >> 6) + 1) * 64);
      padded.set(bytes);
      padded[length] = 0x80;
      const view = new DataView(padded.buffer);
      view.setUint32(padded.length - 4, (length << 3) >>> 0, false);
      view.setUint32(padded.length - 8, Math.floor(length / 536870912), false);
      const w = new Uint32Array(64);
      for (let offset = 0; offset < padded.length; offset += 64) {
        for (let i = 0; i < 16; i++) w[i] = view.getUint32(offset + i * 4, false);
        for (let i = 16; i < 64; i++) {
          const a = w[i - 15];
          const b = w[i - 2];
          const s0 = ((a >>> 7) | (a << 25)) ^ ((a >>> 18) | (a << 14)) ^ (a >>> 3);
          const s1 = ((b >>> 17) | (b << 15)) ^ ((b >>> 19) | (b << 13)) ^ (b >>> 10);
          w[i] = (w[i - 16] + s0 + w[i - 7] + s1) >>> 0;
        }
        let [a, b, c, d, e, f, gg, hh] = h;
        for (let i = 0; i < 64; i++) {
          const S1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
          const ch = (e & f) ^ (~e & gg);
          const t1 = (hh + S1 + ch + K[i] + w[i]) >>> 0;
          const S0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
          const maj = (a & b) ^ (a & c) ^ (b & c);
          const t2 = (S0 + maj) >>> 0;
          hh = gg; gg = f; f = e; e = (d + t1) >>> 0;
          d = c; c = b; b = a; a = (t1 + t2) >>> 0;
        }
        h[0] = (h[0] + a) >>> 0; h[1] = (h[1] + b) >>> 0; h[2] = (h[2] + c) >>> 0; h[3] = (h[3] + d) >>> 0;
        h[4] = (h[4] + e) >>> 0; h[5] = (h[5] + f) >>> 0; h[6] = (h[6] + gg) >>> 0; h[7] = (h[7] + hh) >>> 0;
      }
      const out = new Uint8Array(32);
      const outView = new DataView(out.buffer);
      h.forEach((word, i) => outView.setUint32(i * 4, word, false));
      return out;
    };
    const noEntropy = name => () => {
      throw new Error(`crypto.${name}() is unavailable in a browser function: the QuickJS guest has no CSPRNG, and deriving one from Math.random would produce guessable "random" values. Generate randomness on the fleet path and pass it in with the request.`);
    };
    class Hash {
      constructor(algorithm) {
        const name = String(algorithm).toLowerCase().split("-").join("");
        if (name !== "sha256") {
          throw new Error(`crypto.createHash(${JSON.stringify(algorithm)}) is unavailable in a browser function: only sha256 is implemented in the guest`);
        }
        this._parts = [];
      }
      update(data, encoding) { this._parts.push(typeof data === "string" ? R.Buffer.from(data, encoding) : R.Buffer.from(data)); return this; }
      digest(encoding) {
        const out = R.Buffer.from(sha256(R.Buffer.concat(this._parts)));
        return encoding ? out.toString(encoding) : out;
      }
    }
    class Hmac {
      constructor(algorithm, key) {
        this._key = R.Buffer.from(typeof key === "string" ? R.Buffer.from(key) : key);
        this._algorithm = algorithm;
        this._parts = [];
      }
      update(data, encoding) { this._parts.push(typeof data === "string" ? R.Buffer.from(data, encoding) : R.Buffer.from(data)); return this; }
      digest(encoding) {
        let key = this._key;
        if (key.length > 64) key = R.Buffer.from(sha256(key));
        const padded = R.Buffer.alloc(64);
        padded.set(key);
        const inner = R.Buffer.alloc(64);
        const outer = R.Buffer.alloc(64);
        for (let i = 0; i < 64; i++) { inner[i] = padded[i] ^ 0x36; outer[i] = padded[i] ^ 0x5c; }
        const innerHash = sha256(R.Buffer.concat([inner, R.Buffer.concat(this._parts)]));
        const out = R.Buffer.from(sha256(R.Buffer.concat([outer, R.Buffer.from(innerHash)])));
        return encoding ? out.toString(encoding) : out;
      }
    }
    const timingSafeEqual = (a, b) => {
      if (a.length !== b.length) return false;
      let diff = 0;
      for (let i = 0; i < a.length; i++) diff |= a[i] ^ b[i];
      return diff === 0;
    };
    return {
      createHash: algorithm => new Hash(algorithm),
      createHmac: (algorithm, key) => new Hmac(algorithm, key),
      timingSafeEqual,
      randomBytes: noEntropy("randomBytes"),
      randomUUID: noEntropy("randomUUID"),
      randomFillSync: noEntropy("randomFillSync"),
      getRandomValues: noEntropy("getRandomValues"),
      webcrypto: undefined,
      constants: {},
    };
  });

  // ---- fs ---------------------------------------------------------------
  // The ONLY filesystem a browser artifact can have is the host-op mount
  // (`hive.node-compat.fs-read/v1`, op 16), and only when the artifact's policy
  // allows that op. Reads without it, and every write, throw a NAMED error —
  // never a silent empty file.
  R.defineModule("fs", () => {
    const OP_FS_READ = 16;
    const readFile = async (path, options) => {
      const encoding = typeof options === "string" ? options : options && options.encoding;
      if (!R.ops || typeof R.ops.call !== "function") {
        throw Object.assign(new Error(`ENOENT: no such file or directory, open '${path}' — a browser artifact has no filesystem; bundle the data into the handler or fetch it from the fleet`), { code: "ENOENT", path });
      }
      const value = await R.ops.call(OP_FS_READ, { path: String(path), encoding: encoding || null });
      if (encoding) return String(value);
      return R.Buffer.from(value);
    };
    const denied = name => (...args) => {
      const path = args[0];
      throw Object.assign(new Error(`EROFS: read-only file system, ${name} '${path}' — a browser artifact cannot write files`), { code: "EROFS", path });
    };
    const nosync = name => (...args) => {
      throw Object.assign(new Error(`fs.${name}('${args[0]}') is unavailable in a browser function: host reads cross an ASYNC op boundary, so only the callback/promise forms can exist. Use fs.promises.readFile.`), { code: "ENOSYS" });
    };
    const module = {
      promises: { readFile, writeFile: denied("writeFile"), mkdir: denied("mkdir"), unlink: denied("unlink"), readdir: nosync("readdir"), stat: nosync("stat") },
      readFile(path, options, callback) {
        if (typeof options === "function") { callback = options; options = undefined; }
        readFile(path, options).then(value => callback(null, value), error => callback(error));
      },
      readFileSync: nosync("readFileSync"),
      writeFile(path, _data, _options, callback) {
        const done = typeof callback === "function" ? callback : typeof _options === "function" ? _options : () => {};
        done(Object.assign(new Error(`EROFS: read-only file system, open '${path}'`), { code: "EROFS", path }));
      },
      writeFileSync: denied("writeFileSync"),
      appendFileSync: denied("appendFileSync"),
      mkdirSync: denied("mkdirSync"),
      unlinkSync: denied("unlinkSync"),
      rmSync: denied("rmSync"),
      // `existsSync` answers the question truthfully — there is no such file —
      // instead of throwing out of a guard clause that exists to be answered.
      existsSync: () => false,
      readdirSync: nosync("readdirSync"),
      statSync: nosync("statSync"),
      createReadStream: nosync("createReadStream"),
      createWriteStream: denied("createWriteStream"),
      constants: { F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1 },
    };
    return module;
  });
  R.defineModule("fs/promises", () => R.require("fs").promises);

  // ---- http / https -----------------------------------------------------
  R.defineModule("http", () => R.http);
  R.defineModule("https", () => R.http);
  R.defineModule("http2", () => R.http);

  // ---- process / buffer / console as modules ----------------------------
  R.defineModule("process", () => R.process);
  R.defineModule("buffer", () => ({ Buffer: R.Buffer, constants: { MAX_LENGTH: 0x3fffffff }, atob: g.atob, btoa: g.btoa, SlowBuffer: R.Buffer }));
  R.defineModule("console", () => g.console);
  R.defineModule("punycode", () => ({ toASCII: value => value, toUnicode: value => value, encode: value => value, decode: value => value }));
  R.defineModule("vm", () => ({
    runInNewContext(code, context = {}) {
      const keys = Object.keys(context);
      return Function(...keys, `return (${code});`)(...keys.map(key => context[key]));
    },
    runInThisContext(code) { return Function(`return (${code});`)(); },
    Script: class Script {
      constructor(code) { this.code = code; }
      runInNewContext(context) { return Function(...Object.keys(context || {}), `return (${this.code});`)(...Object.values(context || {})); }
    },
  }));
  R.defineModule("module", () => {
    const module = { builtinModules: [...factories.keys()], createRequire: () => R.require, isBuiltin: name => factories.has(String(name).replace(/^node:/, "")) };
    module.Module = module;
    return module;
  });
  R.defineModule("async_hooks", () => ({
    AsyncLocalStorage: class AsyncLocalStorage {
      // Single-threaded guest, one invocation at a time per artifact: a plain
      // slot is a correct implementation here, not an approximation.
      constructor() { this._store = undefined; }
      run(store, fn, ...args) {
        const previous = this._store;
        this._store = store;
        try { return fn(...args); } finally { this._store = previous; }
      }
      getStore() { return this._store; }
      enterWith(store) { this._store = store; }
      exit(fn, ...args) { return this.run(undefined, fn, ...args); }
    },
    executionAsyncId: () => 1,
    createHook: () => ({ enable() {}, disable() {} }),
  }));
  R.defineModule("worker_threads", () => ({ isMainThread: true, threadId: 0, parentPort: null, Worker: class Worker { constructor() { throw new Error("node:worker_threads.Worker is unavailable in a browser function: an artifact runs in one bounded QuickJS context with no thread of its own"); } } }));
  R.defineModule("zlib", () => {
    const fail = name => () => { throw new Error(`node:zlib.${name}() is unavailable in a browser function: no compression codec is linked into the guest. Compress on the fleet path, or return the payload uncompressed and let the gateway negotiate encoding.`); };
    return { gzip: fail("gzip"), gunzip: fail("gunzip"), deflate: fail("deflate"), inflate: fail("inflate"), gzipSync: fail("gzipSync"), gunzipSync: fail("gunzipSync"), createGzip: fail("createGzip"), createGunzip: fail("createGunzip"), constants: {} };
  });
  R.defineModule("tty", () => ({ isatty: () => false, ReadStream: class {}, WriteStream: class {} }));
  R.defineModule("readline", () => ({ createInterface: () => { throw new Error("node:readline is unavailable in a browser function: there is no stdin"); } }));
  R.defineModule("v8", () => ({ getHeapStatistics: () => ({}), serialize: value => R.Buffer.from(JSON.stringify(value)), deserialize: bytes => JSON.parse(R.Buffer.from(bytes).toString()) }));
  R.defineModule("constants", () => ({}));
  R.defineModule("diagnostics_channel", () => ({ channel: () => ({ publish() {}, subscribe() {}, hasSubscribers: false }) }));

  unsupported("net", "there is no TCP socket in a browser tab. A browser function is a request→response handler; run anything that must bind or dial a socket on the fleet path.");
  unsupported("tls", "there is no TCP socket in a browser tab, so there is nothing to wrap in TLS.");
  unsupported("dns", "the guest cannot resolve names — no resolver and no network are reachable from inside the sandbox.");
  unsupported("dgram", "there is no UDP socket in a browser tab.");
  unsupported("cluster", "a browser artifact is a single bounded context; there are no worker processes to fork.");
  unsupported("child_process", "there is no process table and no shell in a browser tab.");
  unsupported("inspector", "no debugger protocol is exposed from the guest.");
  unsupported("repl", "there is no interactive input in a browser function.");
}

function installRequire(g, R) {
  R.bootMs = Date.now();
  const cache = R.moduleCache;
  const require = function require(specifier) {
    const raw = String(specifier);
    const name = raw.startsWith("node:") ? raw.slice(5) : raw;
    if (cache.has(name)) return cache.get(name);
    const factory = R.moduleFactories.get(name);
    if (factory) {
      // Cache BEFORE invoking so a builtin that requires another builtin
      // (url → querystring) can never recurse into a second instance.
      const exports = factory();
      cache.set(name, exports);
      return exports;
    }
    if (name.startsWith("./") || name.startsWith("../") || name.startsWith("/")) {
      throw Object.assign(new Error(`Cannot find module '${raw}': a browser artifact is ONE self-contained file, so relative requires cannot be resolved. Inline the module into the entry file, or bundle the function before deploying.`), { code: "MODULE_NOT_FOUND" });
    }
    throw Object.assign(new Error(`Cannot find module '${raw}': npm dependencies are not installed in the browser substrate — a browser artifact ships as one file with the Node builtins only (${[...R.moduleFactories.keys()].slice(0, 12).join(", ")}, …). Bundle '${raw}' into the entry file, or drop the browser opt-in so this function serves from the fleet.`), { code: "MODULE_NOT_FOUND" });
  };
  require.resolve = specifier => {
    const name = String(specifier).replace(/^node:/, "");
    if (!R.moduleFactories.has(name)) throw Object.assign(new Error(`Cannot find module '${specifier}'`), { code: "MODULE_NOT_FOUND" });
    return name;
  };
  require.cache = {};
  require.extensions = {};
  require.main = undefined;
  R.require = require;
  g.require = require;

  // process. `env` is EMPTY on purpose and is never populated from the host:
  // project env and secrets must never ship to a donor's browser (the reason
  // the old build scan gave for rejecting `process.` outright — the constraint
  // is real, it just belongs here, not in a build-time grep).
  const emitter = new R.EventEmitter();
  const process = {
    env: { NODE_ENV: "production", HIVE_BROWSER_NODE: "1" },
    argv: ["node", "/artifact.js"],
    argv0: "node",
    execPath: "/usr/bin/node",
    execArgv: [],
    platform: "browser",
    browser: true,
    arch: "wasm",
    pid: 1,
    ppid: 0,
    title: "hive-browser-node",
    version: "v20.0.0-hive-browser",
    versions: { node: "20.0.0", hive_browser: "1", quickjs: "release-sync" },
    release: { name: "node" },
    exitCode: undefined,
    connected: false,
    cwd: () => "/",
    chdir: () => { throw new Error("process.chdir() is unavailable in a browser function: there is no filesystem to be in"); },
    uptime: () => (Date.now() - R.bootMs) / 1000,
    hrtime: Object.assign(previous => {
      const ns = Math.round((Date.now() - R.bootMs) * 1e6);
      const now = [Math.floor(ns / 1e9), ns % 1e9];
      if (!previous) return now;
      const seconds = now[0] - previous[0];
      const nanos = now[1] - previous[1];
      return nanos < 0 ? [seconds - 1, nanos + 1e9] : [seconds, nanos];
    }, { bigint: () => BigInt(Math.round((Date.now() - R.bootMs) * 1e6)) }),
    memoryUsage: () => ({ rss: 0, heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 }),
    nextTick: (fn, ...args) => { Promise.resolve().then(() => fn(...args)); },
    exit: code => { throw new Error(`process.exit(${code ?? 0}) was called: a browser function serves ONE request and cannot terminate its host. Return a response instead.`); },
    abort: () => { throw new Error("process.abort() was called in a browser function"); },
    kill: () => { throw new Error("process.kill() is unavailable in a browser function"); },
    emitWarning: (warning) => { R.log("warn", [String(warning && warning.message || warning)]); },
    on: (...args) => emitter.on(...args),
    once: (...args) => emitter.once(...args),
    off: (...args) => emitter.off(...args),
    removeListener: (...args) => emitter.off(...args),
    emit: (...args) => emitter.emit(...args),
    listeners: (...args) => emitter.listeners(...args),
    stdout: { write: chunk => { R.log("log", [String(chunk).replace(/\n$/, "")]); return true; }, isTTY: false, columns: 80, end() {}, on() {} },
    stderr: { write: chunk => { R.log("error", [String(chunk).replace(/\n$/, "")]); return true; }, isTTY: false, columns: 80, end() {}, on() {} },
    stdin: { read: () => null, on() {}, resume() {}, pause() {}, setEncoding() {}, isTTY: false },
    hasUncaughtExceptionCaptureCallback: () => false,
    setUncaughtExceptionCaptureCallback: () => {},
    allowedNodeEnvironmentFlags: new Set(),
    features: {},
    // Post-mortem log ring: the guest has no host log channel (see installPrimitives).
    get __hiveLogs() { return [...R.logs]; },
  };
  R.process = process;
  g.process = process;
  g.global = g;
  g.__dirname = "/";
  g.__filename = "/artifact.js";
  if (typeof g.globalThis === "undefined") g.globalThis = g;
}

function installHttpBridge(g, R) {
  const lower = headers => {
    const out = {};
    for (const [key, value] of Object.entries(headers || {})) out[String(key).toLowerCase()] = value;
    return out;
  };

  // ---- response state ---------------------------------------------------
  const makeResponse = (ops, done) => {
    const state = { status: 200, headers: {}, chunks: [], ended: false };
    const push = (chunk, encoding) => {
      if (chunk === undefined || chunk === null || chunk === "") return;
      state.chunks.push(typeof chunk === "string" ? R.Buffer.from(chunk, encoding) : R.Buffer.from(chunk));
    };
    const res = {
      // `ops` passthrough: the platform's `(request, ops)` contract keeps
      // working unchanged — this object IS the second argument either way.
      call: (op, payload) => (ops && typeof ops.call === "function"
        ? ops.call(op, payload)
        : Promise.reject(new Error("host operations are unavailable in this substrate"))),
      statusCode: 200,
      statusMessage: "",
      headersSent: false,
      finished: false,
      locals: {},
      setHeader(name, value) { state.headers[String(name).toLowerCase()] = Array.isArray(value) ? value.join(", ") : String(value); return res; },
      getHeader(name) { return state.headers[String(name).toLowerCase()]; },
      getHeaders() { return { ...state.headers }; },
      hasHeader(name) { return Object.prototype.hasOwnProperty.call(state.headers, String(name).toLowerCase()); },
      removeHeader(name) { delete state.headers[String(name).toLowerCase()]; return res; },
      writeHead(code, message, headers) {
        res.statusCode = Number(code);
        const map = typeof message === "object" && message !== null ? message : headers;
        if (typeof message === "string") res.statusMessage = message;
        for (const [key, value] of Object.entries(map || {})) res.setHeader(key, value);
        return res;
      },
      status(code) { res.statusCode = Number(code); return res; },
      sendStatus(code) { res.statusCode = Number(code); return res.end(String(code)); },
      set(name, value) {
        if (name && typeof name === "object") for (const [key, item] of Object.entries(name)) res.setHeader(key, item);
        else res.setHeader(name, value);
        return res;
      },
      header(name, value) { return res.set(name, value); },
      append(name, value) {
        const key = String(name).toLowerCase();
        state.headers[key] = state.headers[key] ? `${state.headers[key]}, ${value}` : String(value);
        return res;
      },
      type(value) {
        const known = { json: "application/json", html: "text/html", text: "text/plain", css: "text/css", js: "application/javascript", xml: "application/xml" };
        return res.setHeader("content-type", known[String(value)] || String(value));
      },
      vary(value) { return res.append("vary", value); },
      cookie(name, value, options = {}) {
        const parts = [`${name}=${encodeURIComponent(value)}`];
        if (options.maxAge !== undefined) parts.push(`Max-Age=${Math.floor(options.maxAge / 1000)}`);
        if (options.path) parts.push(`Path=${options.path}`);
        if (options.httpOnly) parts.push("HttpOnly");
        if (options.secure) parts.push("Secure");
        if (options.sameSite) parts.push(`SameSite=${options.sameSite}`);
        return res.append("set-cookie", parts.join("; "));
      },
      redirect(code, location) {
        if (typeof code === "string") { location = code; code = 302; }
        res.statusCode = Number(code);
        res.setHeader("location", String(location));
        return res.end("");
      },
      json(value) {
        if (!res.hasHeader("content-type")) res.setHeader("content-type", "application/json; charset=utf-8");
        return res.end(JSON.stringify(value === undefined ? null : value));
      },
      send(value) {
        if (value === undefined || value === null) return res.end("");
        if (typeof value === "object" && !(value instanceof Uint8Array)) return res.json(value);
        if (!res.hasHeader("content-type")) {
          res.setHeader("content-type", value instanceof Uint8Array ? "application/octet-stream" : "text/html; charset=utf-8");
        }
        return res.end(value);
      },
      write(chunk, encoding) { res.headersSent = true; push(chunk, encoding); return true; },
      end(chunk, encoding, callback) {
        if (typeof chunk === "function") { callback = chunk; chunk = undefined; }
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        push(chunk, encoding);
        if (!state.ended) {
          state.ended = true;
          res.finished = true;
          res.headersSent = true;
          state.status = Number(res.statusCode) || 200;
          state.headers = { ...state.headers };
          done(state);
        }
        if (callback) callback();
        return res;
      },
      on() { return res; },
      once() { return res; },
      emit() { return false; },
      removeListener() { return res; },
      flushHeaders() { res.headersSent = true; },
      __hiveState: state,
    };
    return { res, state };
  };

  // ---- request ----------------------------------------------------------
  const makeRequest = descriptor => {
    const raw = typeof descriptor === "string" ? JSON.parse(descriptor) : (descriptor || {});
    const headers = lower(raw.headers);
    const rawBody = typeof raw.bodyBase64 === "string" && raw.bodyBase64.length
      ? R.Buffer.from(R.base64Decode(raw.bodyBase64))
      : R.Buffer.from(String(raw.body ?? ""), "utf8");
    const target = String(raw.path ?? "/") || "/";
    const url = new R.URL(target, "http://browser.invalid");
    const query = {};
    for (const [key, value] of url.searchParams.entries()) {
      if (key in query) query[key] = Array.isArray(query[key]) ? [...query[key], value] : [query[key], value];
      else query[key] = value;
    }
    const req = {
      // Platform descriptor fields, verbatim: a `(request, ops)` handler that
      // predates this bridge reads exactly what it always read. `path` KEEPS
      // the platform meaning (path + query, as the gateway sends it); Express
      // narrows it to the pathname inside its own dispatch, where that is what
      // an Express author means by `req.path`.
      method: String(raw.method || "GET").toUpperCase(),
      path: target,
      headers,
      body: typeof raw.body === "string" ? raw.body : undefined,
      bodyBase64: raw.bodyBase64,
      // Node/Express surface.
      url: target,
      originalUrl: target,
      baseUrl: "",
      pathname: url.pathname,
      query,
      params: {},
      rawBody,
      httpVersion: "1.1",
      httpVersionMajor: 1,
      httpVersionMinor: 1,
      complete: true,
      aborted: false,
      socket: { remoteAddress: headers["x-forwarded-for"] || "", remotePort: 0, encrypted: true },
      get(name) { return headers[String(name).toLowerCase()]; },
      header(name) { return headers[String(name).toLowerCase()]; },
      is(type) { return String(headers["content-type"] || "").includes(String(type).replace("*", "")); },
      accepts(type) { return String(headers.accept || "").includes(type) ? type : false; },
      setEncoding() { return req; },
      // The body is already whole, so the stream surface is a replay of one
      // chunk: `req.on("data")`, `pipe`, and `for await` all terminate.
      on(event, fn) {
        if (event === "data" && rawBody.length) Promise.resolve().then(() => fn(rawBody));
        if (event === "end") Promise.resolve().then(() => Promise.resolve().then(fn));
        return req;
      },
      once(event, fn) { return req.on(event, fn); },
      removeListener() { return req; },
      resume() { return req; },
      pause() { return req; },
      pipe(destination) {
        if (rawBody.length) destination.write(rawBody);
        destination.end();
        return destination;
      },
      async *[Symbol.asyncIterator]() { if (rawBody.length) yield rawBody; },
    };
    req.connection = req.socket;
    return req;
  };
  R.makeRequest = makeRequest;

  // ---- http module ------------------------------------------------------
  const STATUS_CODES = {
    200: "OK", 201: "Created", 204: "No Content", 301: "Moved Permanently", 302: "Found",
    304: "Not Modified", 400: "Bad Request", 401: "Unauthorized", 403: "Forbidden",
    404: "Not Found", 405: "Method Not Allowed", 409: "Conflict", 413: "Payload Too Large",
    422: "Unprocessable Entity", 429: "Too Many Requests", 500: "Internal Server Error",
    502: "Bad Gateway", 503: "Service Unavailable", 504: "Gateway Timeout",
  };
  const outbound = name => () => {
    throw new Error(`http.${name}() is unavailable in a browser function: the QuickJS guest has no network stack and no fetch. Call the upstream from the fleet path, or expose it through an allowed host op.`);
  };
  // `createServer` returns a CALLABLE server: `module.exports = server` is a
  // valid handler export, and `server.listen()` is a no-op that resolves the
  // usual `app.listen(PORT)` line at module scope instead of failing the whole
  // artifact at boot — the artifact IS the listener; the gateway is the socket.
  const createServer = (options, listener) => {
    if (typeof options === "function") { listener = options; options = {}; }
    if (typeof listener !== "function") throw new TypeError("http.createServer requires a request listener");
    const server = (req, res) => listener(req, res);
    server.__hiveListener = listener;
    server.listen = (...args) => {
      const callback = args.find(arg => typeof arg === "function");
      R.log("info", ["http server listen() is a no-op in a browser function: the platform delivers each request directly to the listener"]);
      if (callback) Promise.resolve().then(callback);
      return server;
    };
    server.close = callback => { if (callback) Promise.resolve().then(callback); return server; };
    server.address = () => ({ address: "127.0.0.1", family: "IPv4", port: 0 });
    server.on = () => server;
    server.once = () => server;
    server.setTimeout = () => server;
    return server;
  };
  R.http = {
    createServer,
    Server: createServer,
    STATUS_CODES,
    METHODS: ["GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"],
    request: outbound("request"),
    get: outbound("get"),
    Agent: class Agent {},
    globalAgent: {},
  };

  // ---- express ----------------------------------------------------------
  // The one userland framework worth shipping: it is the shape most Node
  // handlers are written in, and it needs no capability the guest lacks.
  const compilePattern = pattern => {
    const names = [];
    const source = String(pattern)
      .split("/")
      .map(part => {
        if (!part) return "";
        if (part === "*") { names.push("0"); return "(.*)"; }
        if (part.startsWith(":")) {
          const optional = part.endsWith("?");
          names.push(part.slice(1, optional ? -1 : undefined));
          return optional ? "(?:([^/]+))?" : "([^/]+)";
        }
        return part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      })
      .join("/");
    return { names, regexp: new RegExp(`^${source || "/"}/?$`) };
  };

  const makeRouter = () => {
    const layers = [];
    const router = async (req, res, done) => {
      let index = 0;
      const next = async error => {
        while (index < layers.length) {
          const layer = layers[index++];
          const pathname = req.pathname || "/";
          const isErrorHandler = layer.handler.length >= 4;
          if (error && !isErrorHandler) continue;
          if (!error && isErrorHandler) continue;
          if (layer.method && layer.method !== req.method) continue;
          // A `use` layer matches its whole SUBTREE and, like Express, runs its
          // handler with the mount prefix stripped so a mounted router's own
          // paths are relative to where it was mounted.
          let restore;
          if (layer.mount) {
            const prefix = layer.path === "/" ? "" : layer.path;
            if (prefix) {
              if (pathname !== prefix && !pathname.startsWith(`${prefix}/`)) continue;
              const inner = pathname.slice(prefix.length) || "/";
              const outerPath = req.pathname;
              const outerBase = req.baseUrl;
              req.pathname = inner;
              req.baseUrl = `${outerBase || ""}${prefix}`;
              restore = () => { req.pathname = outerPath; req.baseUrl = outerBase; };
            }
          } else {
            const match = layer.compiled.regexp.exec(pathname);
            if (!match) continue;
            req.params = {};
            layer.compiled.names.forEach((name, i) => {
              if (match[i + 1] !== undefined) req.params[name] = decodeURIComponent(match[i + 1]);
            });
          }
          // Handing control BACK to this router (a mounted router exhausting
          // its own layers, or any middleware calling next()) must undo the
          // prefix strip first, or the remaining layers here would match a
          // path that has been rewritten out from under them.
          const resume = error2 => {
            if (restore) { restore(); restore = undefined; }
            return next(error2);
          };
          try {
            if (isErrorHandler) return await layer.handler(error, req, res, resume);
            return await layer.handler(req, res, resume);
          } catch (thrown) {
            return resume(thrown);
          } finally {
            if (restore) restore();
          }
        }
        if (done) return done(error);
        if (error) throw error;
        if (!res.__hiveState.ended) {
          res.statusCode = 404;
          res.end(`Cannot ${req.method} ${req.pathname || "/"}`);
        }
        return undefined;
      };
      return next();
    };
    const add = (method, path, handlers, mount) => {
      for (const handler of handlers.flat()) {
        if (typeof handler !== "function") continue;
        layers.push({ method, path: String(path), mount: mount === true, compiled: compilePattern(path), handler });
      }
    };
    router.use = (path, ...handlers) => {
      if (typeof path !== "string") { handlers.unshift(path); path = "/"; }
      add(null, path, handlers, true);
      return router;
    };
    for (const method of ["get", "post", "put", "patch", "delete", "head", "options"]) {
      router[method] = (path, ...handlers) => { add(method.toUpperCase(), path, handlers); return router; };
    }
    router.all = (path, ...handlers) => { add(null, path, handlers); return router; };
    router.route = path => {
      const route = {};
      for (const method of ["get", "post", "put", "patch", "delete", "all"]) {
        route[method] = (...handlers) => { router[method](path, ...handlers); return route; };
      }
      return route;
    };
    router.__hiveRouter = true;
    return router;
  };

  const jsonBody = () => (req, _res, next) => {
    const type = String(req.headers["content-type"] || "");
    if (type.includes("json") && typeof req.body === "string" && req.body.trim()) {
      try { req.body = JSON.parse(req.body); } catch (error) { return next(error); }
    } else if (req.body === undefined && req.rawBody && req.rawBody.length && type.includes("json")) {
      try { req.body = JSON.parse(req.rawBody.toString("utf8")); } catch (error) { return next(error); }
    }
    return next();
  };
  const urlencodedBody = () => (req, _res, next) => {
    const type = String(req.headers["content-type"] || "");
    if (type.includes("x-www-form-urlencoded") && typeof req.body === "string") {
      const params = new R.URLSearchParams(req.body);
      const out = {};
      for (const [key, value] of params.entries()) out[key] = value;
      req.body = out;
    }
    return next();
  };

  const express = () => {
    const router = makeRouter();
    const settings = new Map();
    const app = async (req, res, next) => {
      // Express semantics inside Express: `req.path` is the PATHNAME here.
      req.path = req.pathname || "/";
      req.app = app;
      res.app = app;
      return router(req, res, next);
    };
    app.use = (...args) => { router.use(...args); return app; };
    for (const method of ["get", "post", "put", "patch", "delete", "head", "options", "all"]) {
      app[method] = (path, ...handlers) => {
        if (method === "get" && handlers.length === 0) return settings.get(path);
        router[method](path, ...handlers);
        return app;
      };
    }
    app.set = (key, value) => { settings.set(key, value); return app; };
    app.enable = key => { settings.set(key, true); return app; };
    app.disable = key => { settings.set(key, false); return app; };
    app.enabled = key => settings.get(key) === true;
    app.listen = (...args) => {
      const callback = args.find(arg => typeof arg === "function");
      R.log("info", ["express listen() is a no-op in a browser function: the platform delivers each request straight to the app"]);
      if (callback) Promise.resolve().then(callback);
      return R.http.createServer(app);
    };
    app.__hiveApp = true;
    return app;
  };
  express.json = jsonBody;
  express.urlencoded = urlencodedBody;
  express.text = () => (_req, _res, next) => next();
  express.raw = () => (_req, _res, next) => next();
  express.Router = makeRouter;
  express.static = () => (_req, _res, next) => next();
  R.defineModule("express", () => express);

  // ---- the ONE hook the artifact wrapper calls --------------------------
  // Returns the `(req, res)` pair the deployment entry is invoked with, plus
  // `settle()` for the Node convention where the response is WRITTEN to `res`
  // and the handler returns nothing.
  g.__hive_node_bridge = function (request, ops) {
    R.ops = ops;
    let resolveEnd;
    const ended = new Promise(resolve => { resolveEnd = resolve; });
    const req = makeRequest(request);
    const { res, state } = makeResponse(ops, () => resolveEnd(true));
    const envelope = () => {
      const body = R.Buffer.concat(state.chunks);
      return {
        status: state.status,
        headers: state.headers,
        // bodyBase64 is what the gateway actually decodes (`browser_response`
        // prefers it over `body`), so binary responses survive byte-exact;
        // `body` is the readable mirror for the text case.
        body: body.toString("utf8"),
        bodyBase64: R.base64Encode(body),
      };
    };
    return {
      request: req,
      response: res,
      // The single reconciliation point between the two calling conventions.
      // `returned` is whatever the deployment entry's handler returned:
      //   * a value that is not `res` itself  -> the platform `(request, ops)`
      //     contract, returned verbatim (unchanged behaviour, no `res` needed);
      //   * `undefined` or `res`              -> the Node convention: the
      //     response was (or is being) written on `res`. `return res.json(x)`
      //     is the common Express shape and yields `res`, not `undefined` —
      //     reading only `undefined` here silently mis-typed those handlers'
      //     responses as `{status: <function>}`.
      settle(returned) {
        if (returned !== undefined && returned !== res) return Promise.resolve(returned);
        if (state.ended) return Promise.resolve(envelope());
        return ended.then(envelope);
      },
    };
  };
}

const PARTS = [installPrimitives, installUrl, installBuffer, installModules, installRequire, installHttpBridge];

/// The guest-side runtime as ONE evaluable source string.
export function nodeRuntimeSource() {
  return [
    "var __hive_g = globalThis;",
    "var __hive_R = {};",
    ...PARTS.map(part => `(${part.toString()})(__hive_g, __hive_R);`),
  ].join("\n");
}

/// Wrap a VERIFIED artifact source into the expression the runners evaluate
/// (`globalThis.__hive_handler = (<source>)`), with the Node runtime installed
/// first. The artifact source itself is passed through untouched — its bytes
/// were already size-checked, BLAKE3-verified and policy-digest-matched by
/// `pin()`, and nothing here re-writes them; this only decides what globals
/// exist when they run.
export function wrapArtifactSource(source) {
  if (typeof source !== "string") throw new TypeError("artifact source must be a string");
  return `(function () {\n${nodeRuntimeSource()}\nreturn (\n${source}\n);\n})()`;
}
