const OP_FS_READ = 16;
const encoder = new TextEncoder();
const decoder = new TextDecoder();

function normalizePath(raw) {
  if (typeof raw !== "string" || raw.includes("\0")) throw new Error("file path is invalid");
  const parts = raw.replaceAll("\\", "/").split("/").filter(Boolean);
  if (parts.some(part => part === "..")) throw new Error("file path escapes the virtual root");
  return `/${parts.filter(part => part !== ".").join("/")}`;
}

async function guestMain(requestJson, ops, source) {
  const textDecoder = new TextDecoder();
  const textEncoder = new TextEncoder();
  const base64Decode = text => {
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if (text.length % 4 !== 0) throw new TypeError("invalid base64 request body");
    const padding = text.endsWith("==") ? 2 : text.endsWith("=") ? 1 : 0;
    const out = new Uint8Array(text.length / 4 * 3 - padding);
    let written = 0;
    for (let offset = 0; offset < text.length; offset += 4) {
      const values = [...text.slice(offset, offset + 4)].map(char => char === "=" ? 0 : alphabet.indexOf(char));
      if (values.some(value => value < 0)) throw new TypeError("invalid base64 request body");
      const bits = values[0] << 18 | values[1] << 12 | values[2] << 6 | values[3];
      if (written < out.length) out[written++] = bits >>> 16 & 255;
      if (written < out.length) out[written++] = bits >>> 8 & 255;
      if (written < out.length) out[written++] = bits & 255;
    }
    return out;
  };
  const base64Encode = bytes => {
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let out = "";
    for (let offset = 0; offset < bytes.length; offset += 3) {
      const a = bytes[offset];
      const b = bytes[offset + 1] || 0;
      const c = bytes[offset + 2] || 0;
      out += alphabet[a >>> 2];
      out += alphabet[(a & 3) << 4 | b >>> 4];
      out += offset + 1 < bytes.length ? alphabet[(b & 15) << 2 | c >>> 6] : "=";
      out += offset + 2 < bytes.length ? alphabet[c & 63] : "=";
    }
    return out;
  };
  const Buffer = {
    from(value) {
      if (typeof value === "string") return textEncoder.encode(value);
      return value instanceof Uint8Array ? value : new Uint8Array(value);
    },
    isBuffer(value) { return value instanceof Uint8Array; },
  };

  const fs = {
    promises: {
      async readFile(path, encoding) {
        const value = await ops.call(16, { path: String(path), encoding: encoding || null });
        return encoding ? String(value) : value;
      },
    },
    readFile(path, encoding, callback) {
      if (typeof encoding === "function") { callback = encoding; encoding = null; }
      fs.promises.readFile(path, encoding).then(value => callback(null, value), callback);
    },
  };

  function responseState() {
    const state = { status: 200, headers: {}, body: new Uint8Array(), ended: false };
    const bytes = value => typeof value === "string" ? textEncoder.encode(value) : value instanceof Uint8Array ? value : textEncoder.encode(JSON.stringify(value));
    return {
      state,
      api: {
        status(code) { state.status = Number(code); return this; },
        statusCode: 200,
        setHeader(name, value) { state.headers[String(name).toLowerCase()] = String(value); },
        getHeader(name) { return state.headers[String(name).toLowerCase()]; },
        set(name, value) { this.setHeader(name, value); return this; },
        json(value) { this.setHeader("content-type", "application/json"); state.body = bytes(value); state.ended = true; return this; },
        send(value) { state.body = bytes(value); state.ended = true; return this; },
        end(value = "") { state.body = bytes(value); state.ended = true; return this; },
      },
    };
  }

  function routeMatches(pattern, path) {
    if (pattern === "*" || pattern === path) return true;
    const a = pattern.split("/").filter(Boolean);
    const b = path.split("/").filter(Boolean);
    return a.length === b.length && a.every((part, index) => part.startsWith(":") || part === b[index]);
  }

  function express() {
    const layers = [];
    const app = async (req, res) => {
      let index = 0;
      const next = async error => {
        if (error) throw error;
        const layer = layers[index++];
        if (!layer) return;
        if (layer.method !== "USE" && layer.method !== req.method) return next();
        if (!routeMatches(layer.path, req.path)) return next();
        await layer.handler(req, res, next);
      };
      await next();
    };
    app.use = (path, handler) => {
      if (typeof path === "function") { handler = path; path = "*"; }
      layers.push({ method: "USE", path, handler });
      return app;
    };
    for (const method of ["get", "post", "put", "patch", "delete", "all"]) {
      app[method] = (path, handler) => {
        layers.push({ method: method === "all" ? "USE" : method.toUpperCase(), path, handler });
        return app;
      };
    }
    app.listen = () => { throw new Error("listen() is unavailable in a browser node; export the app instead"); };
    app.__hiveApp = true;
    return app;
  }
  express.json = () => (req, _res, next) => {
    if (typeof req.body === "string" && req.body) req.body = JSON.parse(req.body);
    return next();
  };

  const http = {
    createServer(listener) {
      if (typeof listener !== "function") throw new TypeError("http.createServer requires a listener");
      return {
        __hiveListener: listener,
        listen() { throw new Error("listen() is unavailable in a browser node; export the server instead"); },
      };
    },
  };
  const path = {
    join: (...parts) => `/${parts.join("/").split("/").filter(part => part && part !== ".").join("/")}`,
    basename: value => String(value).replaceAll("\\", "/").split("/").pop(),
    dirname: value => String(value).replaceAll("\\", "/").split("/").slice(0, -1).join("/") || "/",
  };
  const require = name => {
    if (name === "express") return express;
    if (name === "http" || name === "node:http") return http;
    if (name === "fs" || name === "node:fs") return fs;
    if (name === "path" || name === "node:path") return path;
    throw new Error(`module ${name} is unavailable in the browser compatibility lane`);
  };

  const module = { exports: {} };
  Function("require", "module", "exports", "Buffer", source)(require, module, module.exports, Buffer);
  const exported = module.exports?.default || module.exports;
  const listener = exported?.__hiveListener || exported;
  if (typeof listener !== "function") throw new Error("node app must export an express app, http server, or request handler");

  const input = JSON.parse(requestJson);
  const url = new URL(input.path || "/", "https://browser.invalid");
  const rawBody = typeof input.bodyBase64 === "string" ? base64Decode(input.bodyBase64) : textEncoder.encode(String(input.body || ""));
  const req = {
    method: String(input.method || "GET").toUpperCase(),
    url: url.pathname + url.search,
    path: url.pathname,
    query: Object.fromEntries(url.searchParams),
    headers: input.headers || {},
    body: typeof input.body === "string" ? input.body : rawBody,
    rawBody,
  };
  const { state, api } = responseState();
  api.statusCode = state.status;
  await listener(req, api);
  if (api.statusCode !== 200 && state.status === 200) state.status = Number(api.statusCode);
  return JSON.stringify({ status: state.status, headers: state.headers, body: textDecoder.decode(state.body), bodyBase64: base64Encode(state.body) });
}

export class BrowserNodeCompat {
  constructor(runtime, options = {}) {
    if (!runtime || typeof runtime.setOp !== "function") throw new Error("node compatibility requires BrowserFunctionRuntime");
    if (typeof options.blake3 !== "function") throw new Error("node compatibility requires BLAKE3");
    this.runtime = runtime;
    this.blake3 = options.blake3;
    this.mounts = new Map();
    runtime.setOp(OP_FS_READ, async (payload, context) => {
      const files = this.mounts.get(context.digest);
      if (!files) throw new Error("virtual filesystem is not mounted for this artifact");
      const path = normalizePath(payload?.path);
      const bytes = files.get(path);
      if (!bytes) throw new Error(`ENOENT: ${path}`);
      return payload?.encoding ? decoder.decode(bytes) : bytes.slice().buffer;
    });
  }

  async pin(source, files = {}, options = {}) {
    if (typeof source !== "string") throw new TypeError("node app source must be a string");
    const mounted = new Map();
    for (const [path, value] of Object.entries(files)) mounted.set(normalizePath(path), encoder.encode(String(value)));
    const wrapper = `async function(request, ops) { return (${guestMain.toString()})(request, ops, ${JSON.stringify(source)}); }`;
    const digest = this.blake3(encoder.encode(wrapper));
    this.mounts.set(digest, mounted);
    try {
      await this.runtime.pin(digest, wrapper, { ...options, allowedOps: [...new Set([...(options.allowedOps || []), OP_FS_READ])] });
    } catch (error) {
      this.mounts.delete(digest);
      throw error;
    }
    return digest;
  }

  unpin(digest) {
    this.mounts.delete(digest);
    return this.runtime.unpin(digest);
  }
}
