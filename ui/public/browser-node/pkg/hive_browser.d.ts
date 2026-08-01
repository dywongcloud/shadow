/* tslint:disable */
/* eslint-disable */
/**
 * The `ReadableStreamType` enum.
 *
 * *This API requires the following crate features to be activated: `ReadableStreamType`*
 */

export type ReadableStreamType = "bytes";

/**
 * A live browser mesh node. Holds the bound endpoint and the spawned accept
 * loop for its lifetime; dropping it tears the endpoint down.
 */
export class BrowserNode {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Serialized `EndpointAddr` (id + relay/transport hints) a peer needs to
     * dial this browser node.
     *
     * THROWS rather than returning an address with no transport hints. `boot`
     * awaits `online()` so this should not happen, but an address carrying
     * only an id is undialable, and silently returning one moves the failure
     * to a different machine's dial attempt where the real cause is invisible.
     * Fail here, where the node that is not ready can actually be named.
     */
    addrJson(): string;
    /**
     * Pull a complete BLAKE3-addressed asset in bounded chunks. Every reply
     * repeats the immutable total length; the final assembled bytes are hashed
     * before any caller can persist them.
     */
    assetOn(peer_addr_json: string, digest: string): Promise<Uint8Array>;
    /**
     * Boot a node: bind an endpoint against `relay_url` (must be `wss://` from
     * an https page — plain `ws://` is mixed-content-blocked), optionally wire
     * a pkarr `discovery_url` for publish+resolve, and restore identity from
     * `secret_hex` (32-byte ed25519 seed, hex) if given — else generate a fresh
     * one. Spawns the `hive/browser/0` accept loop before returning.
     */
    static boot(relay_url: string, discovery_url?: string | null, secret_hex?: string | null): Promise<BrowserNode>;
    /**
     * Clear every execution capability and gracefully close the iroh endpoint.
     * Idempotent and awaitable; unlike wasm-bindgen's generated `free()`, this
     * waits for QUIC close notifications. An invocation already inside a JS
     * Promise may finish; no not-yet-started invocation can begin after grants
     * are cleared.
     */
    close(): Promise<void>;
    /**
     * Outbound test: dial `peer_addr_json` on `hive/browser/0`, send `msg` as
     * an [`Op::Echo`] request, and return the echoed reply. Proves the browser
     * node's OUTBOUND path (browser → relay → peer) in addition to the
     * accept loop's inbound path.
     */
    echoTo(peer_addr_json: string, msg: string): Promise<string>;
    /**
     * Grant one authenticated endpoint permission to pull one exact asset.
     */
    grantAsset(endpoint_id: string, digest: string): boolean;
    /**
     * Grant one TLS-authenticated iroh endpoint permission to invoke exactly
     * one pinned code digest. The boot-empty map is the execution boundary;
     * future platform admission is its sole production writer.
     */
    grantInvoker(endpoint_id: string, code_digest: string): boolean;
    /**
     * Outbound: dial `peer_addr_json` and ask it to invoke the locally pinned
     * artifact named by `code_digest` against `request_json` (a Lagon-shaped
     * `{method,path,headers,body}` envelope), returning raw response bytes as a
     * UTF-8 string. No executable source crosses the wire.
     */
    invokeOn(peer_addr_json: string, code_digest: string, request_json: string): Promise<string>;
    /**
     * The node's cryptographic identity (64-hex EndpointId = its ed25519
     * public key). Stable across reloads iff booted from the same seed.
     */
    nodeId(): string;
    /**
     * Revoke one endpoint/asset capability; pooled connections re-read it on
     * every chunk, so revocation also stops a transfer already in progress.
     */
    revokeAsset(endpoint_id: string, digest: string): boolean;
    /**
     * Revoke one exact endpoint/digest scope. Idempotent: a valid but absent
     * scope returns `false`; malformed IDs/digests throw without mutation.
     * Existing connections re-read this map for every invoke stream.
     */
    revokeInvoker(endpoint_id: string, code_digest: string): boolean;
    /**
     * The raw 32-byte ed25519 seed, as 64 hex chars — the ONLY way key
     * material leaves this module. Per `docs/browser-node-proposal.md` §2.2,
     * this exists solely so the caller can wrap it with a non-extractable
     * WebCrypto key before it ever touches durable storage; the wasm module
     * itself never persists anything. JS strings cannot be zeroed after use
     * (no mutable-buffer access), so the caller must encrypt this value
     * immediately on receipt and never log or store it bare.
     */
    secretHex(): string;
    /**
     * How many inbound echo requests this node has served — proof the accept
     * path fired, readable from the page after a peer connects.
     */
    servedCount(): bigint;
    /**
     * Register the trusted local reader used by [`Op::AssetGet`]. Registration
     * grants nobody; each caller still needs an exact endpoint/digest scope.
     */
    setAssetHandler(handler: Function): void;
    /**
     * Register the trusted resolver called for every authorized
     * [`Op::Invoke`] request. `handler` is
     * `(codeDigest: string, requestJson: string) => Promise<string>` and must
     * resolve `codeDigest` to a LOCALLY pinned artifact; executable source is
     * never accepted from a peer. Installing a handler grants nobody.
     */
    setInvokeHandler(handler: Function): void;
    /**
     * One JSON blob of everything the status UI needs.
     */
    statusJson(): string;
}

export class IntoUnderlyingByteSource {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    cancel(): void;
    pull(controller: ReadableByteStreamController): Promise<any>;
    start(controller: ReadableByteStreamController): void;
    readonly autoAllocateChunkSize: number;
    readonly type: ReadableStreamType;
}

export class IntoUnderlyingSink {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    abort(reason: any): Promise<any>;
    close(): Promise<any>;
    write(chunk: any): Promise<any>;
}

export class IntoUnderlyingSource {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    cancel(): void;
    pull(controller: ReadableStreamDefaultController): Promise<any>;
}

/**
 * Canonical content address shared by function artifacts and asset bytes.
 */
export function blake3Hex(bytes: Uint8Array): string;

export function on_load(): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_browsernode_free: (a: number, b: number) => void;
    readonly blake3Hex: (a: number, b: number) => [number, number];
    readonly browsernode_addrJson: (a: number) => [number, number, number, number];
    readonly browsernode_assetOn: (a: number, b: number, c: number, d: number, e: number) => any;
    readonly browsernode_boot: (a: number, b: number, c: number, d: number, e: number, f: number) => any;
    readonly browsernode_close: (a: number) => any;
    readonly browsernode_echoTo: (a: number, b: number, c: number, d: number, e: number) => any;
    readonly browsernode_grantAsset: (a: number, b: number, c: number, d: number, e: number) => [number, number, number];
    readonly browsernode_grantInvoker: (a: number, b: number, c: number, d: number, e: number) => [number, number, number];
    readonly browsernode_invokeOn: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => any;
    readonly browsernode_nodeId: (a: number) => [number, number];
    readonly browsernode_revokeAsset: (a: number, b: number, c: number, d: number, e: number) => [number, number, number];
    readonly browsernode_revokeInvoker: (a: number, b: number, c: number, d: number, e: number) => [number, number, number];
    readonly browsernode_secretHex: (a: number) => [number, number];
    readonly browsernode_servedCount: (a: number) => bigint;
    readonly browsernode_setAssetHandler: (a: number, b: any) => [number, number];
    readonly browsernode_setInvokeHandler: (a: number, b: any) => [number, number];
    readonly browsernode_statusJson: (a: number) => [number, number];
    readonly on_load: () => void;
    readonly __wbg_intounderlyingsource_free: (a: number, b: number) => void;
    readonly intounderlyingsource_cancel: (a: number) => void;
    readonly intounderlyingsource_pull: (a: number, b: any) => any;
    readonly __wbg_intounderlyingsink_free: (a: number, b: number) => void;
    readonly intounderlyingsink_abort: (a: number, b: any) => any;
    readonly intounderlyingsink_close: (a: number) => any;
    readonly intounderlyingsink_write: (a: number, b: any) => any;
    readonly __wbg_intounderlyingbytesource_free: (a: number, b: number) => void;
    readonly intounderlyingbytesource_autoAllocateChunkSize: (a: number) => number;
    readonly intounderlyingbytesource_cancel: (a: number) => void;
    readonly intounderlyingbytesource_pull: (a: number, b: any) => any;
    readonly intounderlyingbytesource_start: (a: number, b: any) => void;
    readonly intounderlyingbytesource_type: (a: number) => number;
    readonly ring_core_0_17_14__bn_mul_mont: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h0908a51f6b61a600: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen__convert__closures_____invoke__h40609ae72e5a0370: (a: number, b: number, c: any, d: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h65751fdee90535b7: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__hf42dcc60313e6275: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__hfdd9e855e91b810c: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__habd8d9d2a2106490: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h0d78351b17b6d108: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h2b85946bfc7620d7: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h100cfb892b577986: (a: number, b: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_destroy_closure: (a: number, b: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
