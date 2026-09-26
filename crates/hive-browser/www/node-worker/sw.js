(function () {
    'use strict';

    // The message codec: one framing, every kind, both transports.
    //
    // A call out of the worker becomes exactly one message, answered by exactly one
    // message. The *same* bytes go over both transports — a blocking `XMLHttpRequest`
    // through the service worker for `fs.readFileSync`, and a `postMessage` for
    // `fs.promises.readFile` — and that is deliberate rather than incidental. The async
    // path does not need framing at all (`structuredClone` handles a `Uint8Array` perfectly
    // well), but using it means a framing or error-envelope bug cannot hide on one
    // transport and not the other, which is the failure mode this whole boundary is most
    // exposed to. It also makes the crossing zero-copy: one `ArrayBuffer`, transferred.
    //
    // **This is not multiplexing.** Every message is one self-delimiting frame — one
    // `postMessage`, or one XHR body — correlated by its own `seq`. Nothing is interleaved,
    // there are no windows, no per-kind flow control and no stream ids. The `kind` field is
    // a namespace tag that picks a dispatcher, nothing more. The only real channels in the
    // system are the `MessagePort`s handed to program code, and those arrive as an
    // *attachment* on a reply rather than as anything this layer knows about.
    //
    // Bundled into all three outputs — the worker, the page and the service worker — so
    // nothing here may import a package, touch `Buffer`, reach for node's `path`, or point
    // into `src/worker/`. See ../vfs/entry.ts, which states the same rule for the same
    // reason.
    /**
     * Bumped on any incompatible change to the framing or any op set.
     *
     * Carried in the message *and* in the request URL, because a page and a service worker
     * can be different builds: a stale SW is a normal consequence of a redeploy, and the
     * skew has to be detectable before either side parses a body it may not understand.
     */
    const WIRE_PROTO = 2;
    new TextEncoder();
    new TextDecoder();

    // The service worker's side channel to the page.
    //
    // The service worker cannot answer a request itself — it has no mount table, no
    // providers and no stdio — so it relays: a blocking XHR from the node worker arrives as
    // a `fetch` event, the SW hands the message to the page over a `MessagePort`, the page
    // runs the (async) operation, and the answering message becomes the XHR's response
    // body. The node worker's thread is parked inside `send()` for the whole trip, which is
    // the point.
    //
    // The relay never decodes what it carries and never reads the kind. It is a byte pipe
    // with a session registry, and routing a new kind through it costs nothing here.
    //
    // Two browser facts shape every message below, and both are easy to get wrong:
    //
    //   1. **`MessagePort.postMessage` does not start a stopped service worker;
    //      `ServiceWorker.postMessage` does.** So the page always *attaches* through
    //      `registration.active.postMessage`, never over a port it happens to be holding.
    //      A message sent to a port whose SW instance has been killed is simply lost.
    //   2. **A service worker is evicted when idle (~30s) and loses all module state.** Its
    //      session registry is therefore a cache, never the truth, and arriving at a cold
    //      worker is the *normal* path rather than an edge case. `Rescue` is how it gets
    //      its port back, and only a window client can answer it.
    /** The scope-relative path segment the service worker claims. */
    const SW_PATH_SEGMENT = "__nwm";
    /** Carries a rescue to same-origin pages regardless of control or scope. */
    const SW_BROADCAST_CHANNEL = "node-worker-wire";
    /** ms the service worker waits for a page to re-attach after a restart. */
    const RESCUE_TIMEOUT_MS = 1500;
    /**
     * ms the service worker waits for an answer before failing the request.
     *
     * Refreshed by every {@link SwProgress}, so this is a liveness deadline rather than a
     * limit on how long an operation may take. Kept well inside the browsers' own
     * fetch-event ceilings (Chrome ~5 min, Firefox ~300s) so the failure is ours and legible
     * rather than theirs and opaque.
     */
    const OP_TIMEOUT_MS = 15_000;
    /** HTTP statuses the service worker answers with. 200 means "an op ran", success or not. */
    const SW_STATUS = {
        /** The message carries the result, including an in-band ENOENT. */
        ok: 200,
        /** Protocol skew between the page and the service worker. */
        protoMismatch: 400,
        /** No session attached, rescue failed, or the page detached. */
        noSession: 503,
        /** The host did not answer in time. */
        timeout: 504,
    };

    // The transport half of the bridge: a blocking XHR from the node worker, relayed to the
    // page, answered as an HTTP response.
    //
    // The service worker never looks inside a frame. It moves opaque bytes and does nothing
    // else — no mount table, no providers, no filesystem knowledge at all — which is why this
    // file imports only constants and why it can be a **classic** script. (It has to be: module
    // service workers are not universally shipped, and there is nothing to gain from ESM in
    // 200 lines.)
    //
    // Exported as `installNodeWorkerFetch` rather than being the whole worker, because **only
    // one service worker can own a scope**. A host application that already has one cannot
    // register ours, and without this it could never use synchronous filesystem access at all;
    // with it, they call this from their own worker.
    //
    // Two browser facts shape everything below, and both are easy to get wrong:
    //
    //   1. **`MessagePort.postMessage` does not start a stopped service worker;
    //      `ServiceWorker.postMessage` does.** So the page always *attaches* through
    //      `registration.active.postMessage`, and this side never initiates over a port it
    //      merely happens to be holding. A message to a port whose SW instance has been killed
    //      is silently lost.
    //   2. **A service worker is evicted when idle and loses all module state.** The session
    //      registry below is a cache, never the truth, and arriving at a cold worker is the
    //      *normal* path — in testing Chrome dropped it between two phases of the same run.
    //      `rescue` is how it gets its port back, and only a window client can answer.
    const HEADERS = {
        "Content-Type": "application/octet-stream",
        "Cache-Control": "no-store",
    };
    function installNodeWorkerFetch() {
        const sw = self;
        // Scope-relative, so one build works at any registration scope and the page can compute
        // the identical URL from `registration.scope`. This is what removes the need for a
        // `Service-Worker-Allowed` header, which matters because the deployment target is static
        // hosting that cannot set one.
        const prefix = new URL(`${SW_PATH_SEGMENT}/`, sw.registration.scope).pathname;
        const sessions = new Map();
        const waiting = new Map();
        // Keyed by session *and* sequence, never sequence alone.
        //
        // `nextSeq` restarts at 1 every time this worker is evicted and respawned, which is routine and
        // can happen between any two operations. Keyed on the number by itself, a reply arriving on one
        // session's port could settle a request issued to a different session — and since the frame was
        // then decoded and returned without further checks, the caller got another session's answer as
        // if it were its own. That is how two workers sharing a filesystem read each other's files.
        const pending = new Map();
        let nextSeq = 1;
        const key = (sid, seq) => `${sid}:${seq}`;
        function settle(sid, seq, frame) {
            const k = key(sid, seq);
            const entry = pending.get(k);
            if (!entry)
                return;
            pending.delete(k);
            clearTimeout(entry.timer);
            entry.resolve(frame);
        }
        function expire(sid, seq) {
            const k = key(sid, seq);
            const entry = pending.get(k);
            if (!entry)
                return;
            pending.delete(k);
            entry.reject(new Error("host did not answer in time"));
        }
        function onPortMessage(sid, data) {
            if (!data)
                return;
            if (data.t === "res") {
                settle(sid, data.seq, data.frame);
                return;
            }
            if (data.t === "progress") {
                // A slow operation is not a hung page — a large write into OPFS can legitimately
                // outlast the deadline — so a heartbeat refreshes it rather than raising it for
                // everything.
                const entry = pending.get(key(sid, data.seq));
                if (entry) {
                    clearTimeout(entry.timer);
                    entry.timer = setTimeout(() => expire(sid, data.seq), OP_TIMEOUT_MS);
                }
            }
        }
        function adopt(sid, port, clientId) {
            // Always replaces. From the page's side a stale port is indistinguishable from a live
            // one, so it must be free to mint a replacement at any time — and this side must
            // prefer the newest.
            port.onmessage = (e) => onPortMessage(sid, e.data);
            port.start?.();
            sessions.set(sid, { port, clientId });
            const list = waiting.get(sid);
            if (list) {
                waiting.delete(sid);
                const session = sessions.get(sid);
                for (const resolve of list)
                    resolve(session);
            }
            return clientId;
        }
        sw.addEventListener("message", (event) => {
            const data = event.data;
            if (!data)
                return;
            if (data.t === "attach") {
                const port = event.ports[0];
                if (!port)
                    return;
                const clientId = event.source?.id;
                adopt(data.sid, port, clientId);
                port.postMessage({
                    t: "attached",
                    prefix,
                    proto: WIRE_PROTO,
                    clientId,
                });
                return;
            }
            if (data.t === "detach") {
                sessions.delete(data.sid);
            }
        });
        sw.addEventListener("fetch", (event) => {
            // The respondWith decision must be reachable with NO await, and this handler must
            // never throw for a URL it does not own: with a fetch listener registered, *every*
            // in-scope request arrives here — the 8 MB worker script, every asset beside it, the
            // cross-origin epoxy import, every api call. A plain `return` leaves them to the
            // network untouched.
            //
            // NOT `respondWith(fetch(request))`. That would route the whole asset graph through
            // this thread, degrade byte-range handling (a synthesized 200 for a `Range` request
            // breaks media elements) and put the epoxy wasm's `Content-Type` — which streaming
            // compilation depends on — at the mercy of our response construction. Nothing is
            // cached here either: a cached worker script one build older than the page is a
            // version-skew bug nobody enjoys.
            const url = new URL(event.request.url);
            if (url.origin !== sw.location.origin)
                return;
            if (!url.pathname.startsWith(prefix))
                return;
            if (event.request.method !== "POST")
                return;
            // `serve` must never reject. A rejected `respondWith` reaches a synchronous XHR as an
            // opaque network error ("Failed to load …"), with no way for the worker to say what
            // happened — so every internal failure is turned into a response carrying a reason.
            event.respondWith(serve(url, event.request).catch((err) => new Response(`node-worker: service worker failed: ${err}`, {
                status: SW_STATUS.noSession,
                headers: HEADERS,
            })));
        });
        async function serve(url, request) {
            // `{prefix}v{proto}/{sid}/{seq}-{op}`. The sid has to be in the URL because the
            // respondWith decision happens before the body can be read; `{seq}-{op}` makes the
            // devtools network panel a readable syscall trace.
            const rest = url.pathname.slice(prefix.length).split("/");
            if (rest[0] !== `v${WIRE_PROTO}`) {
                // A page and a service worker can be different builds — a stale worker is a normal
                // consequence of a redeploy — so say so instead of parsing a body neither side
                // agrees on.
                return new Response(`node-worker: service worker speaks v${WIRE_PROTO}, page asked for ${rest[0]} — reload`, { status: SW_STATUS.protoMismatch, headers: HEADERS });
            }
            const sid = rest[1];
            if (!sid) {
                return new Response("node-worker: no session in url", {
                    status: SW_STATUS.noSession,
                    headers: HEADERS,
                });
            }
            // Both of these can fail for reasons that have nothing to do with the filesystem — a
            // body that cannot be read, a `clients` call on a client that has gone — and neither
            // used to be guarded, which is how an internal failure became an unexplained network
            // error at the other end.
            let frame;
            try {
                frame = await request.arrayBuffer();
            }
            catch (err) {
                return new Response(`node-worker: could not read the request body: ${err}`, {
                    status: SW_STATUS.noSession,
                    headers: HEADERS,
                });
            }
            let session;
            try {
                session = sessions.get(sid) ?? (await rescue(sid));
            }
            catch (err) {
                return new Response(`node-worker: session lookup failed: ${err}`, {
                    status: SW_STATUS.noSession,
                    headers: HEADERS,
                });
            }
            if (!session) {
                return new Response("node-worker: no filesystem host attached", {
                    status: SW_STATUS.noSession,
                    headers: HEADERS,
                });
            }
            const seq = nextSeq++;
            try {
                const out = await new Promise((resolve, reject) => {
                    const timer = setTimeout(() => expire(sid, seq), OP_TIMEOUT_MS);
                    pending.set(key(sid, seq), { resolve, reject, timer });
                    session.port.postMessage({ t: "op", seq, frame }, [frame]);
                });
                return new Response(out, { status: SW_STATUS.ok, headers: HEADERS });
            }
            catch (err) {
                return new Response(`node-worker: ${err.message}`, {
                    status: SW_STATUS.timeout,
                    headers: HEADERS,
                });
            }
        }
        /**
         * The registry was empty. Normal, not exceptional — see the note at the top.
         *
         * Every window is asked, plus a broadcast — `BroadcastChannel` from a service worker
         * reaches same-origin pages regardless of control or scope. *This* side initiates,
         * because a message to the page's stale port would go nowhere.
         *
         * There used to be a fast path here: `clients.get(sid.split(".")[0])`, on the premise
         * that a sid is `<pageClientId>.<nonce>`. It never once succeeded. A page cannot know
         * its own client id before it has attached — the id comes back *in* the `attached`
         * reply — so the sid it mints has no dot in it, `clients.get` was handed the whole sid,
         * and every rescue fell through to the code below having first burned a lookup. The
         * premise cannot be satisfied without a second attach round trip at startup, which
         * costs more than the fallback it was avoiding, so the fast path is gone rather than
         * left in place looking like it works.
         */
        async function rescue(sid) {
            {
                let windows = [];
                try {
                    windows = await sw.clients.matchAll({
                        type: "window",
                        // A page whose controller changed under it is still the owner of the session and
                        // still the only thing that can answer.
                        includeUncontrolled: true,
                    });
                }
                catch {
                    // Treated as "nobody to ask", which is the honest reading.
                }
                if (windows.length === 0) {
                    // Nobody could possibly answer. Fail immediately rather than making the worker
                    // wait out a deadline for a reply that is not coming.
                    return undefined;
                }
                for (const client of windows)
                    client.postMessage({ t: "rescue", sid });
                try {
                    new BroadcastChannel(SW_BROADCAST_CHANNEL).postMessage({
                        t: "rescue",
                        sid,
                    });
                }
                catch {
                    // BroadcastChannel is not everywhere; the direct posts above are the main path.
                }
            }
            return new Promise((resolve) => {
                const list = waiting.get(sid) ?? [];
                list.push(resolve);
                waiting.set(sid, list);
                setTimeout(() => resolve(sessions.get(sid)), RESCUE_TIMEOUT_MS);
            });
        }
    }

    // The shipped service worker: `dist/sw.js`.
    //
    // A consumer who already owns their scope should import `installNodeWorkerFetch` from
    // `node-worker/sw-handler` into their own worker instead — only one service worker can own a
    // scope, and this one claims the whole thing.
    const sw = self;
    installNodeWorkerFetch();
    // `skipWaiting` on *install* only, which is the case where there is nothing to displace: a
    // first registration has no existing worker, so taking over immediately is free and saves the
    // page a reload before synchronous filesystem access works.
    //
    // Deliberately NOT on update. A waiting worker takes over once its clients are gone, and
    // forcing it in mid-run would swap the transport under a worker that is parked in a blocking
    // request. Version skew is detected instead: the protocol version rides in both the handshake
    // and the request URL, and a mismatch is answered with a legible error rather than a body
    // neither side agrees on.
    sw.addEventListener("install", (event) => {
        if (!sw.registration.active)
            event.waitUntil(sw.skipWaiting());
    });
    // Claiming is not required for interception — measured across Blink, Gecko and WebKit, a
    // dedicated worker is controlled because its own script URL is in scope, and the page needs no
    // control at all. It is done anyway because it costs nothing and makes a page-side `fetch`
    // probe possible; nothing in the design depends on it.
    sw.addEventListener("activate", (event) => {
        event.waitUntil(sw.clients.claim());
    });

})();
