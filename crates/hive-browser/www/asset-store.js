const DIGEST = /^[0-9a-f]{64}$/;
const CACHE = "hive-browser-assets-v1";
const DIR = "hive-browser-assets-v1";
const PREFIX = "/__hive_asset/";

function bytesOf(value) {
  if (value instanceof Uint8Array) return value;
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  throw new TypeError("asset bytes must be an ArrayBuffer or typed array");
}

export class BrowserAssetStore {
  constructor(options = {}) {
    if (typeof options.blake3 !== "function") throw new Error("asset store requires a BLAKE3 implementation");
    this.blake3 = options.blake3;
    this.cacheName = options.cacheName || CACHE;
    this.directoryName = options.directoryName || DIR;
    this.prefix = options.prefix || PREFIX;
    this.ready = this.open();
  }

  async open() {
    if (!navigator.storage?.getDirectory) throw new Error("OPFS is unavailable");
    if (!navigator.locks?.request) throw new Error("Web Locks are required for asset single-writer safety");
    const root = await navigator.storage.getDirectory();
    this.directory = await root.getDirectoryHandle(this.directoryName, { create: true });
    this.cache = await caches.open(this.cacheName);
  }

  validate(digest) {
    if (!DIGEST.test(digest)) throw new Error("asset digest must be 64 lowercase hexadecimal characters");
  }

  key(digest) {
    return new URL(`${this.prefix}${digest}`, location.origin).href;
  }

  async ingest(value, type) {
    const blob = value instanceof Blob ? value : new Blob([bytesOf(value)]);
    const bytes = new Uint8Array(await blob.arrayBuffer());
    const digest = this.blake3(bytes);
    await this.putVerified(digest, bytes, type || blob.type || "application/octet-stream");
    return { digest, bytes: bytes.byteLength, url: this.key(digest) };
  }

  async putVerified(digest, value, type = "application/octet-stream") {
    await this.ready;
    this.validate(digest);
    const bytes = bytesOf(value);
    if (this.blake3(bytes) !== digest) throw new Error("asset bytes do not match BLAKE3 digest");
    await navigator.locks.request(`hive-asset:${digest}`, async () => {
      const file = await this.directory.getFileHandle(digest, { create: true });
      const writer = await file.createWritable({ keepExistingData: false });
      try {
        await writer.write(bytes);
        await writer.close();
      } catch (error) {
        await writer.abort().catch(() => {});
        throw error;
      }
      const meta = await this.directory.getFileHandle(`${digest}.json`, { create: true });
      const metaWriter = await meta.createWritable({ keepExistingData: false });
      await metaWriter.write(JSON.stringify({ type, bytes: bytes.byteLength }));
      await metaWriter.close();
      await this.cache.put(this.key(digest), new Response(bytes.slice(), {
        headers: {
          "content-type": type,
          "content-length": String(bytes.byteLength),
          "accept-ranges": "bytes",
          "cache-control": "public, max-age=31536000, immutable",
          "x-hive-blake3": digest,
        },
      }));
    });
  }

  async metadata(digest) {
    try {
      const handle = await this.directory.getFileHandle(`${digest}.json`);
      return JSON.parse(await (await handle.getFile()).text());
    } catch (error) {
      if (error?.name === "NotFoundError") return { type: "application/octet-stream" };
      throw error;
    }
  }

  async read(digest, offset = 0, maxLen = Number.MAX_SAFE_INTEGER) {
    await this.ready;
    this.validate(digest);
    if (!Number.isSafeInteger(offset) || offset < 0) throw new Error("asset offset is invalid");
    if (!Number.isSafeInteger(maxLen) || maxLen <= 0) throw new Error("asset range length is invalid");
    let file;
    try {
      file = await (await this.directory.getFileHandle(digest)).getFile();
    } catch (error) {
      if (error?.name === "NotFoundError") throw new Error("asset is not pinned locally");
      throw error;
    }
    if (offset > file.size) throw new Error("asset offset exceeds length");
    const end = Math.min(file.size, offset + maxLen);
    return { total: file.size, bytes: new Uint8Array(await file.slice(offset, end).arrayBuffer()) };
  }

  async get(digest) {
    await this.ready;
    this.validate(digest);
    const hit = await this.cache.match(this.key(digest));
    if (hit) return hit;
    const { total, bytes } = await this.read(digest);
    if (this.blake3(bytes) !== digest) throw new Error("stored asset failed BLAKE3 verification");
    const { type } = await this.metadata(digest);
    await this.cache.put(this.key(digest), new Response(bytes.slice(), {
      headers: { "content-type": type, "content-length": String(total), "accept-ranges": "bytes", "x-hive-blake3": digest },
    }));
    return this.cache.match(this.key(digest));
  }

  attach(node) {
    node.setAssetHandler((digest, offset, maxLen) => this.read(digest, offset, maxLen));
  }

  async pull(node, peerAddrJson, digest, type = "application/octet-stream") {
    this.validate(digest);
    const bytes = bytesOf(await node.assetOn(peerAddrJson, digest));
    await this.putVerified(digest, bytes, type);
    return { digest, bytes: bytes.byteLength, url: this.key(digest) };
  }

  async remove(digest) {
    await this.ready;
    this.validate(digest);
    return navigator.locks.request(`hive-asset:${digest}`, async () => {
      await this.cache.delete(this.key(digest));
      await this.directory.removeEntry(digest).catch(error => {
        if (error?.name !== "NotFoundError") throw error;
      });
      await this.directory.removeEntry(`${digest}.json`).catch(error => {
        if (error?.name !== "NotFoundError") throw error;
      });
    });
  }
}
