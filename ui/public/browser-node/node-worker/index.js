/**
 * How long a flush on a teardown path waits before giving up on it.
 *
 * Long enough that an ordinary flush always finishes inside it, short enough that a reader
 * which has stopped reading cannot hold a dead process open.
 */
const FLUSH_GRACE_MS = 500;
// TODO add backpressure across the worker?
class Console {
    // @internal
    // stdout's other side
    writableOut;
    writableErr;
    // @internal
    // stdin's other side
    readable;
    worker;
    consoleIsTty;
    ttyStateValue = {
        isRaw: false,
        echo: true,
    };
    ttyListeners = new Set();
    stdout;
    stderr;
    stdin;
    constructor(worker, isTTY = true) {
        // A terminal by default, which is what an interactive embedder wants. `false` matters
        // for a worker that is one step of a pipeline rather than something a person is
        // watching: node's console colourises on `getColorDepth()`, which reports truecolor
        // while this is set, so a program whose output is being captured emits ANSI escapes
        // into it and every byte-for-byte comparison downstream fails on an invisible diff.
        this.consoleIsTty = isTTY;
        let { readable: out1, writable: out2 } = new TransformStream();
        this.stdout = out1;
        this.writableOut = out2;
        let { readable: err1, writable: err2 } = new TransformStream();
        this.stderr = err1;
        this.writableErr = err2;
        let { readable: in2, writable: in1 } = new TransformStream();
        this.stdin = in1;
        this.readable = in2;
        this.worker = worker;
    }
    // ------------------------------------------------------------------- stdio
    //
    // The worker's end of these used to be the streams themselves, transferred at startup.
    // It is messages now, which is what lets `readSync(0)` and `writeSync(1)` work at all —
    // a stream can only be read asynchronously, and stdio that cannot be synchronous is
    // stdio node programs cannot use. The embedder's view is unchanged: it still writes
    // `stdin` and reads `stdout`/`stderr`.
    #outWriter;
    #errWriter;
    #inReader;
    /** Bytes read from stdin but not yet asked for. */
    #leftover;
    #stdinEnded = false;
    /** The tail of the write chain, so `flush` can wait for what is already queued. */
    #writes = Promise.resolve();
    /** @internal */
    writeStdio(fd, bytes) {
        const writer = fd === 2
            ? (this.#errWriter ??= this.writableErr.getWriter())
            : (this.#outWriter ??= this.writableOut.getWriter());
        // Chained rather than awaited: a write must not block the message that carried it,
        // and the chain is what keeps the bytes in order anyway.
        this.#writes = this.#writes.then(() => writer.write(bytes), () => writer.write(bytes));
    }
    /**
     * @internal Wait for queued writes to be accepted.
     *
     * `timeoutMs` is for callers on the exit path, and they should all pass one. A chunk
     * settles when whoever is reading `stdout` accepts it, so a reader that stalls holds this
     * open indefinitely — and a flush that cannot finish must not be able to stop a process
     * from being reported as ended. Waiting is a courtesy to the program's last line; the
     * ending is not optional.
     */
    async flushStdio(timeoutMs) {
        const writes = this.#writes.catch(() => { });
        if (timeoutMs === undefined) {
            await writes;
            return;
        }
        await Promise.race([
            writes,
            new Promise((resolve) => setTimeout(resolve, timeoutMs)),
        ]);
    }
    /**
     * @internal End `stdout` and `stderr`, so anything reading them sees the end.
     *
     * Called when the worker is terminated. Without it a reader of `console.stdout` waits for a
     * chunk that cannot come — nothing will ever write again — and that is not a leak the page
     * can see or work around: a `TransformStream` readable ends only when its writable is
     * closed, and only this side holds the writable.
     *
     * Invisible while a page makes one worker and reads it until it closes the tab. It stops
     * being invisible the moment workers are short-lived, which is what a page that runs a
     * worker per child process is: there, "wait for the output to end" is how you know the
     * child is done, and it would simply never resolve.
     *
     * Queued writes are flushed first, and closing a `TransformStream` writable still delivers
     * what is already queued before the reader sees `done` — so this ends the stream without
     * truncating the program's last line.
     */
    async closeStdio() {
        // Bounded: this runs when the worker is being torn down, and the reader it is waiting
        // for may be the very thing that has gone away.
        await this.flushStdio(FLUSH_GRACE_MS);
        const out = (this.#outWriter ??= this.writableOut.getWriter());
        const err = (this.#errWriter ??= this.writableErr.getWriter());
        await Promise.allSettled([out.close(), err.close()]);
    }
    /**
     * @internal
     *
     * `blocking` is the difference between a prompt and a poll. A blocking read waits for
     * input or for the stream to end; a non-blocking one answers with whatever is already
     * here, which may be nothing at all.
     */
    async readStdio(length, blocking) {
        const empty = new Uint8Array(0);
        if (!this.#leftover?.length) {
            if (this.#stdinEnded)
                return { bytes: empty, eof: true };
            if (!blocking)
                return { bytes: empty, eof: false };
            const reader = (this.#inReader ??= this.readable.getReader());
            const { value, done } = await reader.read();
            if (done) {
                this.#stdinEnded = true;
                return { bytes: empty, eof: true };
            }
            this.#leftover = value;
        }
        const held = this.#leftover;
        if (held.length <= length) {
            this.#leftover = undefined;
            return { bytes: held, eof: false };
        }
        this.#leftover = held.subarray(length);
        return {
            bytes: held.subarray(0, length),
            eof: false,
        };
    }
    get isTTY() {
        return this.consoleIsTty;
    }
    get ttyState() {
        return this.ttyStateValue;
    }
    onTTYChange(listener) {
        this.ttyListeners.add(listener);
        listener(this.ttyStateValue);
        return () => {
            this.ttyListeners.delete(listener);
        };
    }
    handleTTYState(state) {
        this.ttyStateValue = {
            ...this.ttyStateValue,
            ...state,
        };
        for (let listener of this.ttyListeners) {
            listener(this.ttyStateValue);
        }
    }
    async setIsTTY(value, size) {
        await this.worker.control({
            op: "ctl.setTty",
            isTTY: value,
            size,
        });
        this.consoleIsTty = value;
    }
    /**
     * Report the terminal's dimensions as `process.stdout.columns`/`rows`.
     *
     * Call it again when the terminal is resized: a CLI that lays out a progress line
     * reads these on every write, so a stale width shows up as wrapped or truncated
     * output rather than as an error.
     */
    async setSize(size) {
        await this.worker.control({
            op: "ctl.setTty",
            isTTY: this.consoleIsTty,
            size,
        });
    }
}

/**
 * The largest message this hands to `send()`, whatever the two ends negotiated.
 *
 * SCTP refuses a message over the negotiated `max-message-size` — Chrome caps at 256
 * KiB, other stacks report less — while what these streams carry is a byte stream with
 * no framing of its own, so a write of any size is legal on this side and has to be cut
 * down. 64 KiB is the size every SCTP stack accepts (RFC 8831 §6.6), so it doubles as
 * the fallback for when the transport cannot be asked.
 */
const SAFE_MESSAGE_SIZE = 65536;
function rtcDataChannelToStreams(pc, dc, { writeHighWaterMark = 1 << 20, // 1 MiB
writeLowWaterMark = writeHighWaterMark >> 1, maxPendingReadBytes = 1 << 20, // hard cap; true inbound backpressure needs app-level flow control
 } = {}) {
    dc.binaryType = "arraybuffer";
    dc.bufferedAmountLowThreshold = writeLowWaterMark;
    const channelError = () => new DOMException("RTCDataChannel is not open", "NetworkError");
    const waitForOpen = () => dc.readyState === "open"
        ? Promise.resolve()
        : new Promise((resolve, reject) => {
            const onOpen = () => done(resolve);
            const onClose = () => done(() => {
                console.warn("[node-worker] [peer] [rtc] datachannel closed before open");
                reject(channelError());
            });
            const onError = (e) => done(() => {
                console.warn("[node-worker] [peer] [rtc] datachannel errored before open", e);
                reject(channelError());
            });
            const done = (fn) => {
                dc.removeEventListener("open", onOpen);
                dc.removeEventListener("close", onClose);
                dc.removeEventListener("error", onError);
                fn();
            };
            dc.addEventListener("open", onOpen, { once: true });
            dc.addEventListener("close", onClose, { once: true });
            dc.addEventListener("error", onError, { once: true });
        });
    // Resolved per write rather than once up front: `pc.sctp` is null until the SCTP
    // transport comes up, which is after this function builds the streams.
    const maxSendSize = () => {
        const negotiated = pc.sctp?.maxMessageSize;
        if (!negotiated || !Number.isFinite(negotiated))
            return SAFE_MESSAGE_SIZE;
        return Math.max(1, Math.min(negotiated, SAFE_MESSAGE_SIZE));
    };
    const waitForWritable = () => dc.bufferedAmount <= writeLowWaterMark
        ? Promise.resolve()
        : new Promise((resolve, reject) => {
            const onLow = () => done(resolve);
            const onClose = () => done(() => {
                console.warn("[node-worker] [peer] [rtc] datachannel closed while waiting to drain");
                reject(channelError());
            });
            const onError = (e) => done(() => {
                console.warn("[node-worker] [peer] [rtc] datachannel errored while waiting to drain", e);
                reject(channelError());
            });
            const done = (fn) => {
                dc.removeEventListener("bufferedamountlow", onLow);
                dc.removeEventListener("close", onClose);
                dc.removeEventListener("error", onError);
                fn();
            };
            dc.addEventListener("bufferedamountlow", onLow, { once: true });
            dc.addEventListener("close", onClose, { once: true });
            dc.addEventListener("error", onError, { once: true });
        });
    let readController = null;
    let readClosed = false;
    let pending = [];
    let pendingBytes = 0;
    const maybeCloseReadable = () => {
        if (readClosed && pending.length === 0 && readController) {
            try {
                readController.close();
            }
            catch (e) {
                console.warn("[node-worker] [peer] [rtc] failed to close readable (likely already errored/cancelled)", e);
            }
            readController = null;
        }
    };
    const drainReads = () => {
        if (!readController)
            return;
        try {
            while (pending.length && (readController.desiredSize ?? 0) > 0) {
                const chunk = pending.shift();
                pendingBytes -= chunk.byteLength;
                readController.enqueue(chunk);
            }
        }
        catch (e) {
            console.warn("[node-worker] [peer] [rtc] failed to enqueue inbound chunk", e);
            readController = null;
            return;
        }
        maybeCloseReadable();
    };
    dc.addEventListener("message", (event) => {
        const chunk = new Uint8Array(event.data);
        pending.push(chunk);
        pendingBytes += chunk.byteLength;
        if (pendingBytes > maxPendingReadBytes) {
            const err = new DOMException("Readable side overflowed; RTCDataChannel cannot apply true inbound backpressure without app-level flow control", "QuotaExceededError");
            console.warn("[node-worker] [peer] [rtc] inbound overflow", err);
            try {
                readController?.error(err);
            }
            catch (e) {
                console.warn("[node-worker] [peer] [rtc] failed to error readable on overflow", e);
            }
            readController = null;
            dc.close();
            return;
        }
        drainReads();
    });
    dc.addEventListener("close", () => {
        console.warn("[node-worker] [peer] [rtc] datachannel closed");
        readClosed = true;
        maybeCloseReadable();
    });
    dc.addEventListener("error", (e) => {
        console.warn("[node-worker] [peer] [rtc] datachannel errored", e);
        try {
            readController?.error(channelError());
        }
        catch (e) {
            console.warn("[node-worker] [peer] [rtc] failed to propagate error to readable", e);
        }
        readController = null;
    });
    const readable = new ReadableStream({
        start(controller) {
            readController = controller;
            drainReads();
        },
        pull() {
            drainReads();
        },
        cancel(reason) {
            console.warn("[node-worker] [peer] [rtc] readable cancelled", reason);
            dc.close();
        },
    }, {
        highWaterMark: writeHighWaterMark,
        size: (chunk) => chunk.byteLength,
    });
    const writable = new WritableStream({
        async write(chunk) {
            const max = maxSendSize();
            // A zero-length chunk sends nothing at all: the loop skips it, which is what a
            // byte stream means by it anyway, and some stacks mishandle empty messages.
            for (let off = 0; off < chunk.byteLength; off += max) {
                while (dc.bufferedAmount > writeHighWaterMark) {
                    await waitForWritable();
                }
                try {
                    // A view, not a copy — `send()` takes any ArrayBufferView.
                    dc.send(chunk.subarray(off, Math.min(off + max, chunk.byteLength)));
                }
                catch (e) {
                    console.warn("[node-worker] [peer] [rtc] datachannel send failed", e);
                    throw e;
                }
            }
            if (dc.bufferedAmount > writeHighWaterMark) {
                await waitForWritable();
            }
        },
        close() {
            dc.close(); // no half-close in RTCDataChannel
        },
        abort(reason) {
            console.warn("[node-worker] [peer] [rtc] writable aborted", reason);
            dc.close();
        },
    });
    return [readable, writable, waitForOpen()];
}
/**
 * The `server.create`/`client.connect` credential, which is one field or the other.
 *
 * `authToken` is a puter token, and the server is listed under that user. `anonToken`
 * is any opaque string, and it is a *shared* address rather than a private credential:
 * the signaller matches a client to a server by the `(anonToken, port)` pair, so
 * whoever holds the token can reach the port. That is how an anonymous server is
 * reached at all — see `previewUrlFor` in the consuming app, and `puter.peer.connect`
 * in browser.js, which dials with `{ port, anonToken }` and no invite code.
 */
function credential(token, anon) {
    return anon ? { anonToken: token } : { authToken: token };
}
async function handlePeerServe(token, port, signaller, iceServers, anon) {
    let conns = new Map();
    let code = `<port ${port}>`;
    let { port1: rx, port2: tx } = new MessageChannel();
    tx.start();
    let ws = new WebSocket(signaller);
    let settleClosed;
    let closed = new Promise((res) => (settleClosed = res));
    let done = false;
    /**
     * Drop everything, from either direction and at most once.
     *
     * Reached three ways: the worker asking over its port (`net.Server.close()`), the
     * owner calling `close()` because the worker is being terminated and can no longer
     * ask, and a failure during setup. Closing the signaller socket is the part that
     * matters beyond this page: while it is open the signaller keeps this `(credential,
     * port)` registered, so a stale one competes with the next server on that port.
     */
    let shutdown = () => {
        if (done)
            return;
        done = true;
        for (let [, peer] of conns)
            peer.close();
        conns.clear();
        tx.close();
        ws.close();
        settleClosed();
    };
    try {
        await new Promise((res, rej) => {
            ws.onopen = () => res();
            ws.onerror = (e) => {
                console.warn("[node-worker] [peer] signaller error", e);
                rej(new Error("Signaller connection errored unexpectedly"));
            };
            ws.onclose = () => rej(new Error("Signaller connection closed unexpectedly"));
        });
        ws.send(JSON.stringify({
            server: {
                create: {
                    ...credential(token, anon),
                    port,
                },
            },
        }));
        let resolve = (_) => {
            throw "unreachable";
        };
        ws.onmessage = async (e) => {
            let msg = JSON.parse(e.data).server;
            if (!msg)
                return;
            if (msg.create) {
                resolve(msg.create);
            }
            else if (msg.connect) {
                try {
                    let id = msg.connect.id;
                    let peer = new RTCPeerConnection({ iceServers });
                    conns.set(id, peer);
                    // A server outlives the connections it accepts, and `conns` is what
                    // `shutdown` closes — so without this, every client that ever
                    // disconnected stays in the map holding an ICE/TURN allocation open for
                    // as long as the server runs.
                    peer.addEventListener("connectionstatechange", () => {
                        if (peer.connectionState !== "closed" &&
                            peer.connectionState !== "failed") {
                            return;
                        }
                        if (conns.get(id) === peer)
                            conns.delete(id);
                        peer.close();
                    });
                    peer.onicecandidate = (e) => {
                        if (!e.candidate)
                            return;
                        ws.send(JSON.stringify({
                            server: {
                                candidate: {
                                    id,
                                    candidate: e.candidate,
                                },
                            },
                        }));
                    };
                    let datachannel = peer.createDataChannel("channel-1", {
                        negotiated: true,
                        id: 2,
                    });
                    let [readable, writable, ready] = rtcDataChannelToStreams(peer, datachannel, { maxPendingReadBytes: Infinity });
                    await ready;
                    tx.postMessage({ readable, writable }, { transfer: [readable, writable] });
                }
                catch (err) {
                    console.warn("[node-worker] [peer] failed to accept client", err);
                }
            }
            else if (msg.candidate) {
                let peer = conns.get(msg.candidate.id);
                if (!peer)
                    return;
                await peer.addIceCandidate(msg.candidate.candidate);
            }
            else if (msg.offer) {
                let id = msg.offer.id;
                let peer = conns.get(id);
                if (!peer)
                    return;
                await peer.setRemoteDescription(new RTCSessionDescription(msg.offer.offer));
                let answer = await peer.createAnswer();
                await peer.setLocalDescription(answer);
                ws.send(JSON.stringify({
                    server: {
                        answer: {
                            id,
                            answer,
                        },
                    },
                }));
            }
        };
        code = await new Promise((res, rej) => {
            resolve = (data) => {
                if (data.success) {
                    // An invite code is how an *authenticated* server is reached, and the
                    // signaller only mints one for that case — an anonymous create answers a
                    // bare `{success:true}`, because its address is the `(anonToken, port)`
                    // pair the client already has. So a missing code is success, not a
                    // half-created server, and the caller keeps it only to report it.
                    res(data.invitecode ?? "");
                }
                else {
                    rej(new Error(`Signaller failed: ${data.error}`));
                }
            };
            setTimeout(() => rej(new Error("Server creation timed out")), 15000);
        });
        ws.onerror = (e) => console.warn("[node-worker] [peer] signaller error", code, e);
        ws.onclose = () => console.warn("[node-worker] [peer] signaller closed", code);
        tx.onmessage = () => shutdown();
        return { code, port: rx, close: shutdown, closed };
    }
    catch (e) {
        shutdown();
        throw e;
    }
}
async function handlePeerConnect(token, target, signaller, iceServers, anon) {
    let code = target.code;
    let peer = new RTCPeerConnection({
        iceServers,
    });
    let datachannel = peer.createDataChannel("channel-1", {
        negotiated: true,
        id: 2,
    });
    let [readable, writable, ready] = rtcDataChannelToStreams(peer, datachannel, {
        maxPendingReadBytes: Infinity,
    });
    let ws = new WebSocket(signaller);
    let settleClosed;
    let closed = new Promise((res) => (settleClosed = res));
    let done = false;
    /**
     * As the server's, and for the same reason — but note what it closes that the old
     * teardown did not: the `RTCPeerConnection` and the signaller socket.
     *
     * Neither had an owner before. Closing the datachannel leaves its connection holding
     * whatever ICE candidates and TURN allocations it gathered, and the socket stayed open
     * for the life of the page, because the only teardown here ran on a *failed* connect —
     * a successful one returned two streams and nothing that could ever close them.
     */
    let shutdown = () => {
        if (done)
            return;
        done = true;
        datachannel.close();
        peer.close();
        ws.close();
        settleClosed();
    };
    // The remote hanging up is a teardown too, and the one that happens most.
    datachannel.addEventListener("close", () => shutdown(), { once: true });
    try {
        // hack??
        if (code)
            code = code.toUpperCase();
        console.debug("[node-worker] [peer] dialing", target.port != null ? `port ${target.port}` : `invite code ${code}`);
        await new Promise((res, rej) => {
            ws.onopen = () => res();
            ws.onerror = (e) => {
                console.warn("[node-worker] [peer] signaller error", e);
                rej(new Error("Signaller connection errored unexpectedly"));
            };
            ws.onclose = () => {
                console.warn("[node-worker] [peer] signaller closed");
                rej(new Error("Signaller connection closed unexpectedly"));
            };
        });
        let wsErrorPromise = new Promise((_, rej) => {
            ws.onerror = (e) => {
                console.warn("[node-worker] [peer] signaller error", e);
                rej(new Error("Signaller connection errored unexpectedly"));
            };
            ws.onclose = () => {
                console.warn("[node-worker] [peer] signaller closed");
                rej(new Error("Signaller connection closed unexpectedly"));
            };
        });
        /*
         * By port when we were given one, by invite code otherwise.
         *
         * The signaller keys a server on `(credential, port)` and mints a code only for an
         * authenticated one, so a port is the address that always exists — see `credential`
         * above. Sending both would be ambiguous; exactly one goes on the wire.
         */
        ws.send(JSON.stringify({
            client: {
                connect: {
                    ...credential(token, anon),
                    ...(target.port != null
                        ? { port: target.port }
                        : { invitecode: code }),
                },
            },
        }));
        peer.onicecandidate = (e) => {
            if (!e.candidate)
                return;
            ws.send(JSON.stringify({
                client: {
                    candidate: {
                        candidate: e.candidate,
                    },
                },
            }));
        };
        let wsPromise = new Promise((_, rej) => {
            ws.onmessage = async (e) => {
                let msg = JSON.parse(e.data).client;
                if (!msg)
                    return;
                if (msg.answer) {
                    await peer.setRemoteDescription(msg.answer.answer);
                }
                else if (msg.candidate) {
                    if (msg.candidate.candidate) {
                        await peer.addIceCandidate(msg.candidate.candidate);
                    }
                }
                else if (msg.connect) {
                    if (msg.connect.success) {
                        let offer = await peer.createOffer();
                        await peer.setLocalDescription(offer);
                        ws.send(JSON.stringify({
                            client: {
                                offer: { offer },
                            },
                        }));
                    }
                    else {
                        rej(new Error(`Signaller failed: ${msg.connect.error}`));
                    }
                }
                else if (msg.disconnect) {
                    rej(new Error(`Signaller sent a disconnect: ${msg.disconnect.reason}`));
                }
            };
        });
        await Promise.race([ready, wsPromise, wsErrorPromise]);
        return { readable, writable, close: shutdown, closed };
    }
    catch (e) {
        shutdown();
        throw e;
    }
}

// Page-side socket.io client for puterfs change notifications.
//
// Puter has no filesystem-watch api, but the backend already pushes
// `item.added` / `item.updated` / `item.removed` / `item.moved` over socket.io
// to every socket joined to the authenticated user's room — the same stream
// puter.js consumes for cache invalidation. That is what backs `fs.watch` in
// the worker.
//
// This lives in the page rather than the worker for three reasons:
//   - the worker's `globalThis.WebSocket` is epoxy's WISP-tunnelled override
//     (worker/epoxy/globals.ts), so a page-side socket is a plain browser one;
//   - one connection is shared by every watcher in every worker on the token,
//     and it survives `execute` boundaries;
//   - it sits outside worker/keepalive.ts accounting, so the socket by itself
//     can never hold a run open — only watchers ref.
//
// The protocol is hand-rolled instead of pulling in socket.io-client: the
// backend is socket.io@4.8 / engine.io@6.6 with `allowEIO3` unset, we only ever
// receive server-to-client events, and websocket-only means none of the polling
// transport or upgrade machinery is reachable.
//
// ## The timestamp fallback
//
// The socket is not always reachable, and the reason is structural rather than
// flaky: the backend's handshake middleware rejects app actors and access-token
// actors outright ("only user tokens accepted"), and puter's launcher hands a
// launched app the *user's* token only in godmode — otherwise it gets an app
// token. On such a launch nothing here ever connects, and until now that meant
// `fs.watch` was silently dead and a filesystem cache had no invalidation source
// at all.
//
// So there is a second, coarse source: `GET /cache/last-change-timestamp`, a
// per-user counter the backend bumps on every `item.*` mutation. It is what
// puter-js polls, it answers for an app actor (it keys off the actor's user),
// and it says only "something changed" — no path, no kind. That is enough to
// invalidate a cache and enough to make a watcher re-check, so it is broadcast
// as `stale` and each consumer decides what revalidating means for it.
//
// Polled only while the socket is *not* delivering, since a live socket is
// strictly better. Both edges of that transition also emit `stale`: a drop and a
// reconnect each leave a window whose events nobody received.
// engine.io packet types (first character of a frame).
const EIO_OPEN = "0";
const EIO_CLOSE = "1";
const EIO_PING = "2";
const EIO_PONG = "3";
const EIO_MESSAGE = "4";
// socket.io packet types (first character of an engine.io MESSAGE payload).
const SIO_CONNECT = "0";
const SIO_DISCONNECT = "1";
const SIO_EVENT = "2";
const SIO_CONNECT_ERROR = "4";
// socket.io-client's own reconnection defaults; there is no reason to be more
// aggressive than the client the backend was built against.
const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 5000;
const RECONNECT_JITTER = 0.5;
/**
 * How often the timestamp endpoint is polled while the socket is down.
 *
 * One small GET, and it replaces what `watchFile` used to do per watched file
 * (a stat each, every 5007 ms) — so this is fewer requests than the fallback it
 * supersedes, not more. It also bounds how stale a cached filesystem answer can
 * be, which is why it is not slower.
 */
const POLL_INTERVAL_MS = 2000;
function coerceBool(value) {
    // The api reports `is_dir` as a boolean on the v2 routes and as 1|0 on the
    // legacy ones, and every `item.*` payload comes from the legacy projection.
    return value === true || value === 1 || value === "1";
}
// Turns one `item.*` payload into the normalized shape the worker consumes.
// Returns null for events that carry no usable path, and for `item.pending`,
// which announces an upload that has not landed yet — the completing write
// emits `item.added`/`item.updated` immediately after, so forwarding both would
// report a file that does not exist yet.
function normalize$1(name, data) {
    if (!data || typeof data.path !== "string" || !data.path)
        return null;
    let isDir = coerceBool(data.is_dir ?? data.isDir);
    // The legacy projection carries all three spellings of the same value; a
    // cache keys on it to notice that an entry it holds under some other path is
    // the one that just moved. See `PuterFsEvent.uid`.
    let raw = data.uid ?? data.uuid ?? data.id;
    let uid = typeof raw === "string" && raw ? raw : undefined;
    switch (name) {
        case "item.added":
            return { kind: "added", path: data.path, isDir, uid };
        case "item.updated":
            return { kind: "updated", path: data.path, isDir, uid };
        case "item.removed":
            return {
                kind: "removed",
                path: data.path,
                isDir,
                uid,
                descendantsOnly: coerceBool(data.descendants_only),
            };
        case "item.moved": {
            // The new FSController emits `from_path`; LegacyFSController and
            // WebDAVController emit `old_path`. Both are live.
            let oldPath = data.from_path ?? data.old_path;
            return {
                kind: "moved",
                path: data.path,
                isDir,
                uid,
                oldPath: typeof oldPath === "string" ? oldPath : undefined,
            };
        }
        // `item.renamed` is deliberately absent: puter-js and the desktop both
        // listen for it, but no backend path has ever emitted it — rename goes
        // out as `item.updated`, naming only the *new* path. That is why `uid` is
        // carried above: it is the only thing tying the two together.
        default:
            return null;
    }
}
// Splits an engine.io MESSAGE payload into its socket.io type and JSON body.
// Wire shape is `<type>[<namespace>,][<ackId>]<json>`; we speak only the
// default namespace and never send an ack-requiring event, but a well-behaved
// parser has to skip both fields rather than assume they are absent.
function parseSocketIoPacket(payload) {
    let type = payload[0];
    let rest = payload.slice(1);
    if (rest.startsWith("/")) {
        let comma = rest.indexOf(",");
        rest = comma === -1 ? "" : rest.slice(comma + 1);
    }
    let digits = 0;
    while (digits < rest.length && rest[digits] >= "0" && rest[digits] <= "9") {
        digits++;
    }
    rest = rest.slice(digits);
    let body = undefined;
    if (rest.length > 0) {
        try {
            body = JSON.parse(rest);
        }
        catch {
            body = undefined;
        }
    }
    return { type, body };
}
class FsEventsHub {
    #token;
    #url;
    #pollUrl;
    /**
     * Worker-bound sinks. Each one posts a push message onto that worker's wire.
     *
     * These used to be `MessagePort`s, and the port was the flaw: a worker parked inside a
     * blocking call never reads one, so a watcher went deaf for the whole of a synchronous
     * loop. A push can ride the reply the worker is already waiting for.
     */
    #sinks = new Set();
    /** In-page consumers, which need no message at all to reach. */
    #listeners = new Set();
    #ws;
    #connected = false;
    #attempt = 0;
    #reconnectTimer;
    #watchdog;
    #pingInterval = 25000;
    #pingTimeout = 20000;
    /** Set when the token itself is bad, which no amount of retrying fixes. */
    #dead = false;
    #onEmpty;
    #pollTimer;
    /** The server's last-change counter as of the previous poll; undefined until the first. */
    #lastChange;
    /** In-flight poll, shared so a burst of `ensureFresh` callers costs one request. */
    #polling;
    /**
     * Whether the counter is actually answering.
     *
     * Reported to consumers so a `watchFile` can tell "coarsely covered" from
     * "covered by nothing", which is the only state where its own stat loop is
     * worth what it costs.
     */
    #pollHealthy = false;
    /**
     * Local time at which this hub last knew it was in step with the server.
     *
     * `Date.now()` whenever the socket is delivering, and the time of the last
     * successful poll otherwise. A consumer holding cached state may trust it for
     * as long as it is willing to be this far behind — the window of unreported
     * change is exactly `Date.now() - checkedAt`.
     */
    #checkedAt = 0;
    constructor(token, apiOrigin, onEmpty) {
        this.#token = token;
        this.#onEmpty = onEmpty;
        let url = new URL("/socket.io/", apiOrigin);
        url.protocol = url.protocol === "http:" ? "ws:" : "wss:";
        url.searchParams.set("EIO", "4");
        url.searchParams.set("transport", "websocket");
        this.#url = url.toString();
        // A GET carrying the token as a query parameter, so it stays a CORS-simple
        // request and costs no preflight — the same reasoning as `PuterApi.#url`.
        let poll = new URL("/cache/last-change-timestamp", apiOrigin);
        if (token)
            poll.searchParams.set("auth_token", token);
        this.#pollUrl = poll.toString();
    }
    /**
     * Hands the worker one end of a channel fed by this hub, and this side the means to
     * take it back.
     *
     * Detaching used to be the worker's move alone — it sends `{type:"close"}` when its
     * last watcher goes away. A *terminated* worker sends nothing, so its port stayed in
     * `#ports` forever, and since the socket lives exactly as long as that set is
     * non-empty, one terminated worker with a watcher kept a socket.io connection open for
     * the life of the page. `NodeWorker.terminate` calls the returned `close`.
     */
    attach(sink) {
        this.#sinks.add(sink);
        if (this.#dead) {
            this.#post(sink, {
                op: "ev.error",
                message: "fs events unavailable: socket auth rejected",
                fatal: true,
            });
        }
        this.#onAttached();
        // Idempotent: detaching a sink that has already gone is a no-op, so this is safe to
        // call after the worker closed the feed itself.
        return {
            connected: this.#connected,
            polling: !this.#connected && this.#pollHealthy,
            close: () => this.#detach(sink),
        };
    }
    /**
     * The same feed, for a consumer living in this page.
     *
     * The filesystem cache is one, and routing it through a `MessageChannel` would
     * mean serializing every event to reach an object three modules away. It refs
     * the hub exactly as `attach` does: the socket lives as long as *anything*
     * wants events, and a cache wants them for the whole session rather than only
     * while something calls `fs.watch`.
     */
    subscribe(fn) {
        this.#listeners.add(fn);
        fn(this.#state());
        this.#onAttached();
        return () => {
            if (!this.#listeners.delete(fn))
                return;
            if (this.#empty)
                this.close();
        };
    }
    get #empty() {
        return this.#sinks.size === 0 && this.#listeners.size === 0;
    }
    #onAttached() {
        if (!this.#dead)
            this.#connect();
        // A `#dead` hub never connects, so polling is the only signal it will ever
        // have — start it here rather than only on a disconnect.
        this.#syncPolling();
    }
    #detach(sink) {
        if (!this.#sinks.delete(sink))
            return;
        if (this.#empty)
            this.close();
    }
    close() {
        this.#clearTimers();
        this.#sinks.clear();
        this.#listeners.clear();
        this.#teardownSocket();
        this.#onEmpty();
    }
    /** Push an event this page produced, rather than one the socket delivered. */
    inject(event) {
        this.#broadcast({ op: "ev.fs", event });
    }
    get connected() {
        return this.#connected;
    }
    /** See `#checkedAt`. */
    freshAsOf() {
        return this.#connected ? Date.now() : this.#checkedAt;
    }
    /**
     * Bring `freshAsOf()` within `maxAgeMs` of now, if it isn't already.
     *
     * A cache calls this before trusting itself. Concurrent callers share the one
     * request, so a burst of cached reads costs a single small GET — which is the
     * whole point of a coarse signal.
     */
    async ensureFresh(maxAgeMs) {
        if (Date.now() - this.freshAsOf() <= maxAgeMs)
            return;
        await this.#poll();
    }
    #state() {
        return {
            op: "ev.state",
            connected: this.#connected,
            polling: !this.#connected && this.#pollHealthy,
        };
    }
    #setPollHealthy(healthy) {
        if (this.#pollHealthy === healthy)
            return;
        this.#pollHealthy = healthy;
        this.#broadcast(this.#state());
    }
    #post(sink, msg) {
        try {
            sink(msg);
        }
        catch {
            // A worker that has gone away is not worth reporting.
        }
    }
    #broadcast(msg) {
        for (let sink of [...this.#sinks])
            this.#post(sink, msg);
        for (let fn of [...this.#listeners]) {
            try {
                fn(msg);
            }
            catch (err) {
                console.warn("[node-worker] [fs-events] listener threw", err);
            }
        }
    }
    // ------------------------------------------------------- the timestamp fallback
    #syncPolling() {
        // A live socket reports every change with a path attached, which is
        // strictly more than this can say. Poll only when it isn't.
        let wanted = !!this.#token && !this.#empty && !this.#connected;
        if (wanted === (this.#pollTimer !== undefined))
            return;
        if (!wanted) {
            clearInterval(this.#pollTimer);
            this.#pollTimer = undefined;
            this.#setPollHealthy(false);
            return;
        }
        this.#pollTimer = setInterval(() => void this.#poll(), POLL_INTERVAL_MS);
        void this.#poll();
    }
    #poll() {
        return (this.#polling ??= this.#pollOnce().finally(() => {
            this.#polling = undefined;
        }));
    }
    async #pollOnce() {
        if (!this.#token)
            return;
        let timestamp;
        try {
            let res = await fetch(this.#pollUrl, { method: "GET" });
            if (!res.ok)
                throw new Error(`HTTP ${res.status}`);
            let body = await res.json();
            timestamp = Number(body?.timestamp);
            if (!Number.isFinite(timestamp))
                throw new Error("no timestamp in body");
        }
        catch (err) {
            // `#checkedAt` deliberately does not advance: a consumer bounding its
            // staleness by it must stop trusting itself while this is failing,
            // which is the correct answer to "I cannot tell whether anything moved".
            console.warn("[node-worker] [fs-events] last-change poll failed", err);
            this.#setPollHealthy(false);
            return;
        }
        let first = this.#lastChange === undefined;
        let moved = !first && timestamp !== this.#lastChange;
        this.#lastChange = timestamp;
        this.#checkedAt = Date.now();
        this.#setPollHealthy(true);
        // The first poll establishes the baseline. Reporting it as a change would
        // throw away a cache that is not yet known to be wrong.
        if (moved)
            this.#broadcast({ op: "ev.stale", timestamp });
    }
    /**
     * Announce a window nobody was listening through.
     *
     * Both edges of the socket's connectivity qualify. A drop is obvious. A
     * *reconnect* is the less obvious one: the gap between the last poll and the
     * socket coming live is unobserved by either source, and the socket sends no
     * backlog.
     */
    #announceGap() {
        this.#broadcast({ op: "ev.stale", timestamp: Date.now() });
    }
    #clearTimers() {
        if (this.#reconnectTimer !== undefined)
            clearTimeout(this.#reconnectTimer);
        if (this.#watchdog !== undefined)
            clearTimeout(this.#watchdog);
        if (this.#pollTimer !== undefined)
            clearInterval(this.#pollTimer);
        this.#reconnectTimer = undefined;
        this.#watchdog = undefined;
        this.#pollTimer = undefined;
    }
    #teardownSocket() {
        let ws = this.#ws;
        this.#ws = undefined;
        if (!ws)
            return;
        ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
        try {
            ws.close();
        }
        catch {
            // Already closing.
        }
    }
    #setConnected(connected) {
        if (this.#connected === connected)
            return;
        let hadBaseline = this.#lastChange !== undefined;
        this.#connected = connected;
        this.#broadcast(this.#state());
        if (!connected) {
            // The counter was not being watched while the socket was, so there is no
            // value to compare the first post-drop poll against — it returns whatever
            // the world is at now, gap included. Assume the worst. Announced *before*
            // polling resumes, so that first poll reads as a baseline rather than as a
            // second, duplicate change.
            this.#announceGap();
            this.#lastChange = undefined;
            this.#syncPolling();
            return;
        }
        this.#syncPolling();
        if (!hadBaseline) {
            // Nothing covered the stretch before the socket came up, so anything
            // learned during it is suspect.
            this.#announceGap();
            return;
        }
        // There *is* a baseline: one more poll answers precisely whether the window
        // between the last one and the socket coming live contained a change. Worth
        // a request — this fires exactly when a session has finished resolving its
        // modules, and announcing a gap unconditionally would throw that away every
        // single startup.
        void this.#poll();
    }
    #connect() {
        if (!this.#token)
            return;
        if (this.#ws || this.#dead || this.#empty)
            return;
        let ws;
        try {
            ws = new WebSocket(this.#url);
        }
        catch (err) {
            console.warn("[node-worker] [fs-events] socket construction failed", err);
            this.#scheduleReconnect();
            return;
        }
        this.#ws = ws;
        ws.onmessage = (e) => {
            if (typeof e.data !== "string")
                return;
            this.#onFrame(ws, e.data);
        };
        ws.onerror = () => {
            // `error` is always followed by `close`, which does the reconnecting.
            // Engine.io gives no detail here beyond "the socket failed".
            console.warn("[node-worker] [fs-events] socket error");
        };
        ws.onclose = () => {
            if (this.#ws !== ws)
                return;
            this.#teardownSocket();
            this.#setConnected(false);
            this.#scheduleReconnect();
        };
    }
    #onFrame(ws, frame) {
        if (frame.length === 0)
            return;
        let type = frame[0];
        let payload = frame.slice(1);
        if (type === EIO_PING) {
            // engine.io v4 has the *server* ping and the client pong.
            ws.send(EIO_PONG);
            this.#armWatchdog();
            return;
        }
        if (type === EIO_OPEN) {
            this.#onHandshake(ws, payload);
            return;
        }
        if (type === EIO_CLOSE) {
            // The server is shutting the transport down; `onclose` follows.
            return;
        }
        if (type === EIO_MESSAGE) {
            this.#onMessage(payload);
            return;
        }
        // PONG/UPGRADE/NOOP: we never ping and never upgrade, so nothing to do.
    }
    #onHandshake(ws, payload) {
        let handshake;
        try {
            handshake = JSON.parse(payload);
        }
        catch {
            console.warn("[node-worker] [fs-events] malformed handshake");
            this.#teardownSocket();
            this.#scheduleReconnect();
            return;
        }
        this.#pingInterval = handshake.pingInterval ?? 25000;
        this.#pingTimeout = handshake.pingTimeout ?? 20000;
        this.#armWatchdog();
        // The backend reads the token from `socket.handshake.auth.auth_token`
        // and joins the socket to the user's room; there is no cookie or query
        // fallback. This is the socket.io CONNECT packet for the default
        // namespace with `auth` as its payload.
        ws.send(EIO_MESSAGE + SIO_CONNECT + JSON.stringify({ auth_token: this.#token }));
    }
    #armWatchdog() {
        if (this.#watchdog !== undefined)
            clearTimeout(this.#watchdog);
        // A silent socket is worse than a closed one: no `close` event fires
        // when the connection is black-holed, so watchers would sit believing
        // they are live. Give the server one full interval plus its own timeout
        // before declaring the transport dead.
        this.#watchdog = setTimeout(() => {
            console.warn("[node-worker] [fs-events] ping timeout, reconnecting");
            this.#teardownSocket();
            this.#setConnected(false);
            this.#scheduleReconnect();
        }, this.#pingInterval + this.#pingTimeout);
    }
    #onMessage(payload) {
        let { type, body } = parseSocketIoPacket(payload);
        if (type === SIO_CONNECT) {
            this.#attempt = 0;
            this.#setConnected(true);
            return;
        }
        if (type === SIO_CONNECT_ERROR) {
            // Auth is checked once, in the handshake middleware, so a rejection
            // here will reject identically forever. Retrying a bad token is what
            // puter.js gets wrong (it registers no `connect_error` handler at
            // all) — stop, and tell the worker why.
            let message = (body && (body.message || body.data?.reason)) ?? "socket auth failed";
            console.warn("[node-worker] [fs-events] connect error", body);
            this.#dead = true;
            this.#clearTimers();
            this.#teardownSocket();
            this.#setConnected(false);
            this.#broadcast({
                op: "ev.error",
                message: `fs events unavailable: ${message}`,
                fatal: true,
            });
            // `#clearTimers` took the poller down with the reconnect timer, and this
            // is the one state where the poller is the *only* source there will ever
            // be — a non-godmode app launch gets an app token, which the handshake
            // middleware refuses. Bring it back up.
            this.#syncPolling();
            // `#setConnected(false)` was a no-op here: this socket never *became*
            // connected, so nothing transitioned. The gap is real all the same, and
            // announcing it is what tells a consumer to establish a baseline now
            // rather than after the first change it would otherwise miss.
            this.#announceGap();
            return;
        }
        if (type === SIO_DISCONNECT) {
            this.#setConnected(false);
            this.#teardownSocket();
            this.#scheduleReconnect();
            return;
        }
        if (type === SIO_EVENT) {
            if (!Array.isArray(body) || typeof body[0] !== "string")
                return;
            let event = normalize$1(body[0], body[1]);
            if (event)
                this.#broadcast({ op: "ev.fs", event });
        }
    }
    #scheduleReconnect() {
        if (this.#dead || this.#empty)
            return;
        if (this.#reconnectTimer !== undefined)
            return;
        let backoff = Math.min(RECONNECT_BASE_MS * 2 ** this.#attempt, RECONNECT_MAX_MS);
        this.#attempt++;
        let delay = backoff * (1 + RECONNECT_JITTER * (Math.random() * 2 - 1));
        this.#reconnectTimer = setTimeout(() => {
            this.#reconnectTimer = undefined;
            this.#connect();
        }, delay);
    }
}
// One hub per (token, origin). Several NodeWorkers on the same account share a
// single socket; each gets its own port.
let hubs = new Map();
/**
 * Fan a locally-produced mutation out to every attached watcher.
 *
 * The filesystem lives on this side now, so its own mutations have no socket echo to wait for.
 * The session that *caused* one already learns about it on the reply frame of the call it made —
 * that is what keeps write-then-observe immediate even while a worker is parked in a blocking
 * call — so this exists for the other direction: telling everyone *else* sharing those providers.
 *
 * A no-op when nothing is watching, since a hub only exists once a watcher subscribes.
 */
function broadcastLocalFsEvent(event) {
    for (const hub of hubs.values())
        hub.inject(event);
}
function hubFor(token, apiOrigin) {
    // NUL as the separator, because it is the one byte that cannot appear in either half,
    // so no origin/token pair can collide with another by splitting differently. Written as
    // an escape rather than the literal byte it used to be: an embedded NUL makes grep and
    // ripgrep classify this file — and every bundle built from it — as binary, and skip it.
    let key = `${apiOrigin}\0${token}`;
    let hub = hubs.get(key);
    if (!hub) {
        hub = new FsEventsHub(token, apiOrigin, () => {
            if (hubs.get(key) === hub)
                hubs.delete(key);
        });
        hubs.set(key, hub);
    }
    return hub;
}
function handleFsEvents(token, apiOrigin, sink) {
    return hubFor(token, apiOrigin).attach(sink);
}
/**
 * Subscribe to the feed from inside the page.
 *
 * Same hub, same socket, same refcount as `handleFsEvents` — this exists because
 * the host filesystem's cache lives three modules away rather than across a
 * worker boundary, and serializing every event through a `MessageChannel` to
 * reach it would be pure ceremony.
 */
function subscribeFsEvents(token, apiOrigin, handler) {
    let hub = hubFor(token, apiOrigin);
    let detach = hub.subscribe(handler);
    return {
        close: detach,
        get connected() {
            return hub.connected;
        },
        freshAsOf: () => hub.freshAsOf(),
        ensureFresh: (maxAgeMs) => hub.ensureFresh(maxAgeMs),
    };
}

// The one error envelope, for every kind of message.
//
// `structuredClone` of an `Error` preserves `name`, `message`, `stack` and `cause`
// and **nothing else** — so `code`, `errno`, `syscall` and `path` are all silently
// dropped. That matters more here than anywhere: `catch (e) { if (e.code !== "ENOENT")
// throw e }` is the dominant idiom in this tree (../worker/node/fs/ops.ts,
// .../handle.ts, .../module/resolve.ts, .../node-core/internal-binding/modules.ts, …),
// and an error that arrives codeless turns every one of those into a rethrow.
//
// So an error is carried as an explicit envelope, and the message travels
// **verbatim**. Composing it is node's business, `formatFsMessage` is the one place
// that does it, and both sides call it — a host that re-rendered the string and a
// worker that re-rendered it again would drift the first time either changed.
//
// This lives under `src/wire/` rather than `src/vfs/` because it is not the
// filesystem's envelope any more: a `WireError` is how *any* kind reports a failure —
// process, stdio, peer, control — carried in-band on the reply so a thrown error can
// never discard the sidebands riding alongside it. The errno table comes with it
// because `toWireError` needs it to coerce an unrecognized throw; ../vfs/errno.ts
// keeps only what a *provider* throws (`VfsError`, `fsError`) and imports the rest
// from here.
/**
 * libuv's negative errno and node's bare message for each code we can produce.
 *
 * The negative numbers are what `err.errno` must be: node's own `ERR_FS_*` paths and
 * the `internal-binding/uv.ts` consumers read it, not just `code`.
 */
const ERRNO = {
    EPERM: { errno: -1, message: "operation not permitted" },
    ENOENT: { errno: -2, message: "no such file or directory" },
    EIO: { errno: -5, message: "i/o error" },
    EBADF: { errno: -9, message: "bad file descriptor" },
    EACCES: { errno: -13, message: "permission denied" },
    EBUSY: { errno: -16, message: "resource busy or locked" },
    EEXIST: { errno: -17, message: "file already exists" },
    EXDEV: { errno: -18, message: "cross-device link not permitted" },
    ENOTDIR: { errno: -20, message: "not a directory" },
    EISDIR: { errno: -21, message: "illegal operation on a directory" },
    EINVAL: { errno: -22, message: "invalid argument" },
    EMFILE: { errno: -24, message: "too many open files" },
    EAGAIN: { errno: -11, message: "resource temporarily unavailable" },
    EFBIG: { errno: -27, message: "file too large" },
    ENOSPC: { errno: -28, message: "no space left on device" },
    EROFS: { errno: -30, message: "read-only file system" },
    ENOSYS: { errno: -38, message: "function not implemented" },
    ENOTEMPTY: { errno: -39, message: "directory not empty" },
    ENOTSUP: { errno: -95, message: "operation not supported" },
    // Linux aliases ENOTSUP and EOPNOTSUPP to the same value, and so does libuv.
    EOPNOTSUPP: { errno: -95, message: "operation not supported" },
};
/**
 * The exact string node puts in `err.message`: `"ENOENT: no such file or directory,
 * stat '/x'"`.
 *
 * Both the worker's `createFsError` and the host's `fsError` route through here so
 * the two can never disagree, which is what lets `WireError.message` be trusted
 * verbatim instead of rebuilt on arrival.
 */
function formatFsMessage(code, message, syscall, path) {
    return `${code}: ${message}${syscall ? `, ${syscall}` : ""}${path ? ` '${path}'` : ""}`;
}
/**
 * Whether something carries the fs-error brand, from any bundle.
 *
 * Structural rather than `instanceof` on purpose — see the note on `VfsError` in
 * ../vfs/errno.ts, which is what sets the brand.
 */
function isVfsError(err) {
    return (typeof err === "object" &&
        err !== null &&
        err.__nodeWorkerFsError === 1);
}
/**
 * Pack any thrown value for the wire.
 *
 * An unrecognized throw becomes **EIO** rather than travelling as an uncoded error.
 * That is deliberate: node's `fs` never throws without a `code`, so a caller doing
 * `if (e.code !== "ENOENT") throw e` would rethrow past a handler that should have
 * caught it, and a caller doing `if (e.code === "EEXIST")` would silently take the
 * wrong branch. `name` and `message` still come through, so the original failure is
 * legible even though its code is synthesized.
 */
function toWireError(err, fallbackSyscall) {
    if (isVfsError(err)) {
        return {
            name: err.name || "Error",
            message: err.message,
            code: err.code,
            errno: err.errno,
            syscall: err.syscall ?? fallbackSyscall,
            path: err.path,
            dest: err.dest,
            stack: err.stack,
        };
    }
    // Anything with a code we recognize is treated as an fs error even without the
    // brand, so a provider that hand-rolls `Object.assign(new Error(...), {code})`
    // still round-trips.
    const e = err;
    if (e && typeof e.code === "string" && ERRNO[e.code]) {
        return {
            name: e.name || "Error",
            message: e.message ?? formatFsMessage(e.code, ERRNO[e.code].message),
            code: e.code,
            errno: typeof e.errno === "number" ? e.errno : ERRNO[e.code].errno,
            syscall: e.syscall ?? fallbackSyscall,
            path: e.path,
            dest: e.dest,
            stack: e.stack,
        };
    }
    const name = (e && e.name) || "Error";
    const message = (e && e.message) || String(err);
    // An AbortError keeps its own message and stays uncoded — callers check `err.name`
    // for it, and inventing an errno would make it look like a disk failure.
    if (name === "AbortError") {
        return { name, message, stack: e && e.stack };
    }
    return {
        name,
        // EIO, *and* the original text.
        //
        // The code has to be EIO so the `if (e.code !== "ENOENT") throw e` idiom behaves,
        // but the message must keep saying what actually went wrong. Reporting a bare
        // "EIO: i/o error" — which this did at first — turns every unexpected failure into
        // the same unreadable line, with the real cause reachable only through a `stack`
        // that is not attached by default. That is a bad trade: nobody debugging a
        // `TypeError` out of a provider is helped by being told it was I/O.
        message: `${formatFsMessage("EIO", ERRNO.EIO.message, fallbackSyscall)} (${name}: ${message})`,
        code: "EIO",
        errno: ERRNO.EIO.errno,
        syscall: fallbackSyscall,
        stack: e && e.stack,
    };
}
/**
 * Rebuild a thrown error from its envelope, in the exact shape
 * `../worker/node/fs/util.ts`'s `createFsError` produces — message verbatim, and
 * `code`/`errno`/`syscall`/`path`/`dest` as own properties.
 *
 * `includeHostStack` is off by default: appending the host's stack is a debugging
 * aid, and a program that prints `err.stack` should not routinely see two.
 */
function fromWireError(e, includeHostStack = false) {
    const err = new Error(e.message);
    err.name = e.name || "Error";
    if (e.code) {
        err.code = e.code;
        err.errno = typeof e.errno === "number" ? e.errno : ERRNO[e.code]?.errno;
    }
    if (e.syscall)
        err.syscall = e.syscall;
    if (e.path)
        err.path = e.path;
    if (e.dest)
        err.dest = e.dest;
    if (includeHostStack && e.stack) {
        err.stack = `${err.stack}\n    --- host ---\n${e.stack}`;
    }
    return err;
}

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
/**
 * `"NWM1"`, as a little-endian u32 — so the first four bytes read as ASCII in a hex
 * dump.
 *
 * This field is the difference between a legible failure and a baffling one. When the
 * service worker has been unregistered, the blocking XHR is answered by the *real*
 * server: a 404 page, or the SPA's `index.html`. Without a magic number that HTML
 * reaches `JSON.parse` and surfaces as a `SyntaxError` from nowhere. With it, the
 * worker reports "not a node-worker message (service worker gone?)".
 */
const FRAME_MAGIC = 0x314d574e;
const HEADER_OFFSET = 16;
/** Round up to the payload's alignment. */
function align8(n) {
    return (n + 7) & -8;
}
/** Thrown by {@link decodeFrame}. Distinguishable, because it means the plumbing broke. */
class FrameError extends Error {
    constructor(message) {
        super(message);
        this.name = "FrameError";
    }
}
const encoder$1 = new TextEncoder();
const decoder$1 = new TextDecoder();
/**
 * ```
 * off  size  field
 *  0    4    magic       "NWM1", little-endian
 *  4    2    proto       WIRE_PROTO
 *  6    2    kind        which dispatcher; see ./kinds.ts, plus SIDEBAND_BIT
 *  8    4    headerLen   bytes of UTF-8 JSON
 * 12    4    payloadLen  sum of every part's length
 * 16    H    header      JSON
 *      pad   to an 8-byte boundary
 *           payload      the parts, concatenated
 * ```
 *
 * `payloadLen` is redundant with the header's `parts` array, and that is the point: a
 * body truncated in transit would otherwise produce a **silently short file**, which
 * is the worst failure mode a filesystem has. Checked against the actual byte length,
 * truncation is an error instead.
 *
 * The 8-byte payload alignment lets a part be viewed as any typed array without
 * copying it first.
 */
/**
 * High bit of the kind field: this message carries others.
 *
 * A flag rather than something read out of the header, because the alternative is parsing
 * every message's JSON twice — once to find out whether there is a sideband and once to
 * dispatch it. The filesystem alone sends tens of thousands of these and almost none of
 * them carry anything, so the common case has to be a bit test.
 */
const SIDEBAND_BIT = 0x8000;
function encodeFrame(header, parts, kind = 0) {
    const headerBytes = encoder$1.encode(JSON.stringify(header));
    const payloadStart = align8(HEADER_OFFSET + headerBytes.length);
    let payloadLen = 0;
    if (parts)
        for (const part of parts)
            payloadLen += part.length;
    const out = new Uint8Array(payloadStart + payloadLen);
    const view = new DataView(out.buffer);
    view.setUint32(0, FRAME_MAGIC, true);
    view.setUint16(4, WIRE_PROTO, true);
    view.setUint16(6, kind, true);
    view.setUint32(8, headerBytes.length, true);
    view.setUint32(12, payloadLen, true);
    out.set(headerBytes, HEADER_OFFSET);
    let at = payloadStart;
    if (parts) {
        for (const part of parts) {
            out.set(part, at);
            at += part.length;
        }
    }
    return out;
}
/**
 * Which dispatcher a message belongs to, read without decoding it.
 *
 * Cheap on purpose: the filesystem alone sends tens of thousands of these, and routing
 * one must not cost a `JSON.parse` of its header.
 *
 * A buffer too short to hold a header has no kind to report. It answers 0 and lets
 * {@link decodeFrame} produce the real diagnostic — every caller decodes immediately
 * after routing, so the malformed message is one line away from a `FrameError` that
 * says what is actually wrong with it.
 */
function frameKind(bytes) {
    return rawKind(bytes) & ~SIDEBAND_BIT;
}
/** Whether anything is riding this message, without decoding it. */
function hasSideband(bytes) {
    return (rawKind(bytes) & SIDEBAND_BIT) !== 0;
}
function rawKind(bytes) {
    const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    if (u8.length < HEADER_OFFSET)
        return 0;
    return new DataView(u8.buffer, u8.byteOffset, u8.byteLength).getUint16(6, true);
}
/**
 * The inverse, with every check that distinguishes "the handler said no" from "the
 * bridge broke". The `parts` are **views** into `bytes`, not copies.
 */
function decodeFrame(bytes) {
    if (!bytes)
        throw new FrameError("empty response");
    const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    if (u8.length < HEADER_OFFSET) {
        throw new FrameError(`response too short (${u8.length} bytes)`);
    }
    const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
    if (view.getUint32(0, true) !== FRAME_MAGIC) {
        throw new FrameError("not a node-worker message (service worker gone or unregistered?)");
    }
    const proto = view.getUint16(4, true);
    if (proto !== WIRE_PROTO) {
        throw new FrameError(`protocol mismatch: message is v${proto}, this build speaks v${WIRE_PROTO} — reload the page`);
    }
    const kind = view.getUint16(6, true) & ~SIDEBAND_BIT;
    const headerLen = view.getUint32(8, true);
    const payloadLen = view.getUint32(12, true);
    const payloadStart = align8(HEADER_OFFSET + headerLen);
    if (payloadStart + payloadLen !== u8.length) {
        throw new FrameError(`truncated message: expected ${payloadStart + payloadLen} bytes, got ${u8.length}`);
    }
    let header;
    try {
        header = JSON.parse(decoder$1.decode(u8.subarray(HEADER_OFFSET, HEADER_OFFSET + headerLen)));
    }
    catch (err) {
        throw new FrameError(`malformed message header: ${err.message}`);
    }
    const lengths = header?.parts ?? (payloadLen ? [payloadLen] : []);
    const parts = [];
    let at = payloadStart;
    for (const length of lengths) {
        parts.push(u8.subarray(at, at + length));
        at += length;
    }
    if (at !== u8.length) {
        throw new FrameError(`message parts do not cover the payload: ${at - payloadStart} of ${payloadLen} bytes`);
    }
    return { header, parts, kind };
}

// Sidebands: messages that ride another message.
//
// The far side cannot always be *reached*. A worker parked inside a blocking XHR will
// never read a `postMessage`, so anything the host wants to tell it — a watch event, a
// cache invalidation — has to arrive on the reply it is already waiting for. The same
// holds outbound: a worker with buffered output that is about to park should send those
// bytes *with* the request rather than racing it, because after the park nothing of its
// own runs until the answer comes back.
//
// That is also what makes stdio affordable. `process.stdout.write` does not pay for a
// round trip of its own: the bytes ride whatever message was going anyway, and only a
// program that prints without doing anything else ever sends one on its own.
//
// The payload stays one flat array — the primary's `n` parts first, then each push's `np`
// in order — so ./frame.ts never has to know sidebands exist. It slices by
// `header.parts` and this module decides who the slices belong to.
/** Everything a sideband adds to a header, given the primary's own parts. */
function withSideband(header, primary, outbound) {
    const parts = [...(primary ?? [])];
    if (primary?.length)
        header.n = primary.length;
    if (outbound?.pushes.length) {
        header.n = primary?.length ?? 0;
        parts.push(...outbound.parts);
    }
    if (parts.length)
        header.parts = parts.map((p) => p.length);
    return { header, parts };
}
function packRequest(kind, seq, call, parts, outbound) {
    const header = { seq, call };
    if (outbound?.pushes.length)
        header.out = outbound.pushes;
    const packed = withSideband(header, parts, outbound);
    return encodeFrame(packed.header, packed.parts, outbound?.pushes.length ? kind | SIDEBAND_BIT : kind);
}
/**
 * Split a decoded message's parts between the message itself and its sidebands.
 *
 * `n` is absent on a message with no sidebands, in which case every part is the primary's
 * — which is what makes this backward-compatible with a header that never heard of them.
 */
function unpackSidebands(header, parts) {
    const pushes = header.out ?? header.push;
    if (!pushes?.length)
        return { primary: parts, sidebands: [] };
    const n = header.n ?? parts.length;
    const primary = parts.slice(0, n);
    const sidebands = [];
    let at = n;
    for (const push of pushes) {
        const count = push.np ?? 0;
        sidebands.push({ push, parts: parts.slice(at, at + count) });
        at += count;
    }
    return { primary, sidebands };
}
/**
 * Just the message's own parts, with any sideband's stripped off.
 *
 * Every decode site needs this and none of them may skip it. A `writeFile` that happens to
 * carry buffered stdout would otherwise see two parts where it expects one — and
 * `fdWritev`, which writes *all* of them, would write the terminal's bytes into the file.
 */
function primaryParts(header, parts) {
    // The overwhelmingly common case is no sideband at all, and it must not cost a copy.
    if (!header.out?.length && !header.push?.length)
        return parts;
    return parts.slice(0, header.n ?? parts.length);
}
/**
 * A queue of messages waiting for a ride.
 *
 * Draining is all-or-nothing per message and the queue is emptied by the drain, so a push
 * is delivered exactly once — there is no acknowledgement and no retry, because the
 * carrier message already has both.
 */
class OutboundQueue {
    #pushes = [];
    #parts = [];
    /**
     * Called at the start of every drain, for a producer that coalesces.
     *
     * Stdout is the reason. A program printing ten thousand lines would otherwise enqueue
     * ten thousand pushes, each with its own header entry — several hundred kilobytes of
     * JSON to carry the same bytes. Contributing at drain time instead lets the producer
     * hand over one run per fd, however many writes went into it.
     */
    onDrain;
    /** Whether a drain right now would produce nothing. Does not run `onDrain`. */
    get empty() {
        return this.#pushes.length === 0;
    }
    enqueue(kind, op, args, parts) {
        this.#pushes.push({ kind, op, args, np: parts?.length ?? 0 });
        if (parts?.length)
            this.#parts.push(...parts);
    }
    drain() {
        this.onDrain?.();
        if (!this.#pushes.length)
            return undefined;
        const out = { pushes: this.#pushes, parts: this.#parts };
        this.#pushes = [];
        this.#parts = [];
        return out;
    }
}

// The reply record that makes a synchronous retry exactly-once.
//
// A synchronous send retries on a transport failure with the **same** `seq` (see
// `SYNC_RETRIES` in ../worker/node/fs/transport.ts). That is only safe because the host
// answers a repeat from this record instead of running the operation a second time —
// without it a retried `append` appends twice, and a retried `proc.spawnSync` runs the
// program again, which with a real shell behind it is a command executed two or three
// times for one call.
//
// This lives in `wire/` rather than in the filesystem's dispatcher because every
// sync-capable kind needs it, not just `fs`. `KIND_PROCESS` and `KIND_CHAN` are both in
// `SYNC_CAPABLE` (./kinds.ts) and both carry side effects a caller would hate to see twice.
/**
 * Frames already answered, keyed by session and then by sequence number.
 *
 * **Per session, not global**, and a session is a *worker* rather than a filesystem. Sequence
 * numbers are minted per worker and start from 1, so a record shared between two workers answers
 * one with the other's reply. That used to be the same thing — one `NodeVfs` backed one worker —
 * but a vfs may back several, and then the two diverge: every worker on it opens at seq 1 and
 * collides with its predecessor from the first request onward. The symptom is not a failure but
 * *wrong data*, indistinguishable from a correct answer, which is the worst kind a filesystem has.
 *
 * Bounded, and small on purpose: it only has to cover an immediate retry, not history. The window
 * is per session too, so a busy worker cannot evict a quiet one's entries out from under it.
 */
const REPLAY_WINDOW = 64;
function createReplayCache() {
    return new Map();
}
function recall(cache, sid, seq) {
    return cache.get(sid)?.get(seq);
}
function remember(cache, sid, seq, frame) {
    let window = cache.get(sid);
    if (!window) {
        window = new Map();
        cache.set(sid, window);
    }
    window.set(seq, frame);
    // Insertion-ordered, so the oldest key is the first one.
    for (const key of window.keys()) {
        if (window.size <= REPLAY_WINDOW)
            break;
        window.delete(key);
    }
}
/** Drop a session's record once its worker is gone, so the map does not grow with the session count. */
function forgetReplays(cache, sid) {
    cache.delete(sid);
}

// Which dispatcher a message belongs to, and whether it can be asked synchronously.
//
// A kind is a *namespace*, not a channel — see the note at the top of ./frame.ts. It
// picks the dispatcher on the far side and nothing else: there is no per-kind ordering,
// buffering or flow control, and two kinds share a transport the way two URLs share a
// socket.
//
// Sync capability is declared here, once, rather than discovered at the call site. The
// hard part of calling out of a worker is doing it *synchronously* — a blocking XHR
// answered by the service worker, with the worker's thread parked for the whole trip —
// and that machinery is expensive to build but free to reuse. What it cannot do is
// carry a `MessagePort` or a `ReadableStream`, because an XHR body is bytes. So a kind
// whose replies hand back a handle is async-only, and the transport rejects a sync send
// of one with ENOSYS instead of hanging on a reply that can never come.
const KIND_FS = 0;
const KIND_PROCESS = 1;
const KIND_STDIO = 2;
const KIND_CHAN = 3;
const KIND_CONTROL = 4;
const KIND_PEER = 5;
const KIND_EVENTS = 6;
/** Diagnostics only — the network panel, and the text of a routing failure. */
const KIND_NAMES = {
    [KIND_FS]: "fs",
    [KIND_PROCESS]: "proc",
    [KIND_STDIO]: "io",
    [KIND_CHAN]: "chan",
    [KIND_CONTROL]: "ctl",
    [KIND_PEER]: "peer",
    [KIND_EVENTS]: "ev",
};
function kindName(kind) {
    return KIND_NAMES[kind] ?? `kind${kind}`;
}

// Routing a message to its dispatcher, and building the answer.
//
// Both halves used to exist twice. Routing was a pair of near-identical ternaries in
// lib/index.ts — one on the service-worker path, one on the postMessage path — which is
// how they came to disagree about which session id to dispatch under, splitting one
// worker's replay record in two. Reply building was a pair of `reply()` functions, one
// per dispatcher, which is how the process side came to set `parts` by hand at every
// call site while the filesystem derived it.
//
// One table and one builder, so a third kind is a `register()` call rather than another
// branch in two places that have to stay in step.
/**
 * Encode an answering message.
 *
 * `parts` are the payload; their lengths are derived here rather than at each call
 * site, which is the half the process dispatcher used to get wrong. Sidebands are
 * attached only when they carry something, so an idle reply stays small.
 */
function encodeReply(kind, body, parts, side) {
    if (side) {
        if (side.events?.length)
            body.events = side.events;
        if (side.invalidate)
            body.invalidate = side.invalidate;
        if (side.apiCalls && Object.keys(side.apiCalls).length) {
            body.apiCalls = side.apiCalls;
        }
    }
    if (parts && parts.length)
        body.parts = parts.map((p) => p.length);
    return encodeFrame(body, parts, kind);
}
/**
 * Encode a failure.
 *
 * `seq` must be the *message's* own sequence number. Answering with anything else — a
 * transport-local counter, say — produces a reply the worker rejects as a crossed
 * response, which reports the plumbing rather than the failure that actually happened.
 */
function encodeErrorReply(kind, seq, err, syscall) {
    return encodeFrame({ seq, result: { ok: false, error: toWireError(err, syscall) } }, undefined, kind);
}
/**
 * Build a dispatcher from a plain `(call) => answer` function.
 *
 * Decode, dispatch, encode, and turn a throw into an in-band `WireError` — every kind
 * needs those four and none of them differ between kinds. Writing them out per dispatcher
 * is how the process side ended up deriving `parts` by hand and answering an undecodable
 * message with a syscall name the filesystem had chosen.
 *
 * `syscallOf` names the operation in an error, so `err.syscall` reads as node's would.
 */
function makeDispatcher(kind, handle, syscallOf, 
/*
 * The exactly-once record for a sync-capable kind, read per call so a caller can hand over
 * a session id it does not know at registration time.
 *
 * Only kinds in `SYNC_CAPABLE` need this, and only because the blocking transport retries a
 * failed send with the *same* `seq`: without a record the handler runs a second and third
 * time for one call. A kind that is async-only can leave it undefined — nothing will ever
 * repeat a seq at it.
 */
replay) {
    return async (frame, attachments) => {
        let request;
        let parts;
        try {
            const decoded = decodeFrame(frame);
            request = decoded.header;
            parts = primaryParts(decoded.header, decoded.parts);
        }
        catch (err) {
            // Nothing to echo, so answer with seq 0: it matches no outstanding request, which
            // makes the sender report its own diagnostic rather than trusting a fabricated one.
            return { frame: encodeErrorReply(kind, 0, err) };
        }
        const record = replay?.();
        const already = record && recall(record.cache, record.sid, request.seq);
        if (already)
            return { frame: already };
        let answer;
        try {
            const answered = (await handle(request.call, parts, attachments)) ?? {};
            answer = encodeReply(kind, {
                seq: request.seq,
                result: { ok: true, value: answered.value ?? null },
            }, answered.parts);
            // A reply carrying a handle is not replayable: the handle is transferred, so a
            // second answer from the record would hand over a stream already detached.
            if (answered.transfer)
                return { frame: answer, transfer: answered.transfer };
        }
        catch (err) {
            answer = encodeErrorReply(kind, request.seq, err, syscallOf?.(request.call));
        }
        if (record)
            remember(record.cache, record.sid, request.seq, answer);
        return { frame: answer };
    };
}
/**
 * Best-effort `seq` for a message that could not be handed to a dispatcher.
 *
 * Answering with the wrong `seq` is worse than answering with 0: the worker checks it
 * and reports a crossed reply, burying the real reason. 0 never matches an outstanding
 * request, so the sender falls back to its own diagnostic — the honest outcome when the
 * message was unroutable in the first place.
 */
function seqOf(frame) {
    try {
        const { header } = decodeFrame(frame);
        return typeof header?.seq === "number" ? header.seq : 0;
    }
    catch {
        return 0;
    }
}
/**
 * Answer a message that could not be handled at all.
 *
 * Takes the *message* rather than a seq, because the seq is the thing most easily got
 * wrong here: a transport that answers with its own counter produces a reply the worker
 * rejects as a crossed response, reporting the plumbing instead of the failure. The kind
 * comes from the message too, so a broken process message is not answered as a
 * filesystem one.
 */
function replyToBrokenFrame(frame, err, syscall) {
    return encodeErrorReply(frameKind(frame), seqOf(frame), err, syscall);
}
/**
 * The kind → dispatcher table, shared by every inbound path.
 *
 * The same instance answers messages arriving over the service-worker relay, over the
 * dedicated message port, and over the bootstrap channel — which is the point. Three
 * arrival paths that route independently are three chances to route differently.
 */
class Router {
    #kinds = new Map();
    /**
     * Route every message riding this one, ignoring failures.
     *
     * A push cannot be reported on — there is nobody waiting for it — so a handler that
     * throws is logged by the handler itself or not at all. What must not happen is a
     * sideband failure taking down the carrier, which is a real message with a real caller.
     */
    #deliverSidebands(frame) {
        // One bit test on the common path. Almost no message carries anything, and decoding
        // every one of them to find that out would double the JSON parsing on the hot path.
        if (!hasSideband(frame))
            return;
        let sidebands;
        try {
            const decoded = decodeFrame(frame);
            if (!decoded.header?.out?.length)
                return;
            sidebands = unpackSidebands(decoded.header, decoded.parts).sidebands;
        }
        catch {
            // The carrier's own dispatcher will report the decode failure properly.
            return;
        }
        for (const { push, parts } of sidebands) {
            const dispatcher = this.#kinds.get(push.kind);
            if (!dispatcher)
                continue;
            void Promise.resolve()
                .then(() => dispatcher(encodeFrame({ seq: 0, call: { op: push.op, ...push.args } }, parts, push.kind), []))
                .catch(() => {
                // Nothing is waiting on a push, so there is nowhere to report this.
            });
        }
    }
    register(kind, dispatcher) {
        if (this.#kinds.has(kind)) {
            throw new Error(`wire: dispatcher for ${kindName(kind)} already registered`);
        }
        this.#kinds.set(kind, dispatcher);
    }
    has(kind) {
        return this.#kinds.has(kind);
    }
    /**
     * Route one message and answer it. Never rejects.
     *
     * A message for a kind nobody registered is answered with ENOSYS rather than
     * dropped: dropping it parks the worker until a timeout it cannot distinguish from
     * a hung page, and the sender is owed the difference.
     */
    async handle(frame, attachments = []) {
        // Anything riding this message is delivered first, and separately.
        //
        // First because a sideband is by definition older than its carrier: the bytes a
        // program printed before it called `readFileSync` were queued before that call was
        // made, and the terminal has to see them in that order. Separately because a push has
        // no reply — it rode here precisely because there was no round trip to give it.
        this.#deliverSidebands(frame);
        const kind = frameKind(frame);
        const dispatcher = this.#kinds.get(kind);
        if (!dispatcher) {
            return {
                frame: encodeErrorReply(kind, seqOf(frame), Object.assign(new Error(`ENOSYS: no handler for ${kindName(kind)} messages, this build registered none`), { code: "ENOSYS" })),
            };
        }
        try {
            const out = await dispatcher(frame, attachments);
            return out instanceof Uint8Array ? { frame: out } : out;
        }
        catch (err) {
            // A dispatcher is supposed to pack its own failures in-band; reaching here
            // means one threw past that, and the worker is still waiting either way.
            return { frame: encodeErrorReply(kind, seqOf(frame), err) };
        }
    }
}

// One end of the message port, in either direction.
//
// Both sides need the same four things: mint a sequence number, park a caller on a reply,
// route what arrives, and fail everything outstanding when the other end goes away. That
// used to be written out twice — `src/worker/conn.ts` and the top of `src/lib/index.ts`
// were the same ninety lines with the identifiers changed, down to the comment explaining
// why errors are packed rather than posted — and the two had already drifted: one dropped
// an unmatched reply silently, the other threw "unreachable!!" at an unregistered type,
// and neither ever timed out or cleaned up after a peer that vanished.
//
// It is one class now, and correlation is the message's own `seq` rather than a uid minted
// alongside it. Each side counts its own requests; a message carrying `call` is a request
// to route, and one carrying `result` is a reply to settle. The two spaces never collide
// because a side only ever looks up seqs it issued itself.
/**
 * An `ArrayBuffer` exactly covering `u8`, without copying when it already does.
 *
 * `encodeFrame` always allocates exactly, so the copy is normally skipped. The guard is
 * there because a view into a larger buffer would otherwise transfer the whole thing.
 */
function asArrayBuffer(u8) {
    return u8.byteOffset === 0 && u8.byteLength === u8.buffer.byteLength
        ? u8.buffer
        : u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);
}
class PortEndpoint {
    router = new Router();
    /**
     * Messages waiting for something to ride on.
     *
     * Drained into every request this side sends, whichever transport carries it — which is
     * what makes buffered stdout free: it goes out with the next `readFileSync` rather than
     * paying for a round trip of its own, and it is *ahead* of that call in the same
     * message, so the terminal cannot see the two out of order.
     */
    outbound = new OutboundQueue();
    #port;
    #seq = 1;
    #inflight = new Map();
    #closed;
    /** Whether there is a port to send on at all. */
    get attached() {
        return !!this.#port;
    }
    attach(port) {
        this.#port = port;
        port.onmessage = (e) => this.#receive(e);
        port.start?.();
    }
    /** The next sequence number this side will use. Exposed for the sync transport. */
    nextSeq() {
        return this.#seq++;
    }
    /**
     * Send a request and wait for its reply.
     *
     * `seq` is minted here so that the sync and async transports share one counter — the
     * host's replay record is keyed on it, and two counters would let a retry over one
     * transport be answered from the other's record.
     */
    call(kind, call, opts = {}) {
        const seq = this.#seq++;
        return this.callWithSeq(seq, kind, call, opts);
    }
    callWithSeq(seq, kind, call, opts = {}, via) {
        return new Promise((resolve, reject) => {
            if (this.#closed) {
                reject(this.#closed);
                return;
            }
            const post = via ?? this.#poster();
            if (!post) {
                reject(new Error("wire: no message port attached"));
                return;
            }
            const bytes = packRequest(kind, seq, call, opts.parts, this.outbound.drain());
            const frame = asArrayBuffer(bytes);
            this.#inflight.set(seq, { resolve, reject });
            const envelope = { f: frame };
            // Transfers first so `attachments[0]` is still the handle every existing reader
            // expects; clones after.
            if (opts.transfer?.length || opts.attach?.length) {
                envelope.a = [...(opts.transfer ?? []), ...(opts.attach ?? [])];
            }
            try {
                post(envelope, [frame, ...(opts.transfer ?? [])]);
            }
            catch (err) {
                this.#inflight.delete(seq);
                reject(err);
            }
        });
    }
    #poster() {
        const port = this.#port;
        if (!port)
            return undefined;
        return (envelope, transfer) => port.postMessage(envelope, transfer);
    }
    /**
     * The one call that cannot go over the port, because it is what delivers the port.
     *
     * Sent over the target's own `postMessage` with the port as its first attachment. The
     * *reply* comes back over that port like every other, so this is a bootstrap only in
     * how it leaves — attach first, then send, or the answer arrives at a port nobody is
     * listening on.
     */
    bootstrap(target, kind, call, transfer) {
        return this.callWithSeq(this.#seq++, kind, call, { transfer }, (envelope, list) => target.postMessage(envelope, list));
    }
    /**
     * Send without waiting for an answer.
     *
     * For a notification whose reply carries nothing worth having — buffered output on its
     * way out, `ctl.exit` from a worker that is about to be terminated. The far side still
     * answers; the reply simply finds no waiter and is dropped.
     */
    post(kind, call, opts = {}) {
        const port = this.#port;
        if (!port || this.#closed)
            return;
        const bytes = packRequest(kind, this.#seq++, call, opts.parts, this.outbound.drain());
        const frame = asArrayBuffer(bytes);
        const envelope = { f: frame };
        if (opts.transfer?.length)
            envelope.a = opts.transfer;
        try {
            port.postMessage(envelope, [frame, ...(opts.transfer ?? [])]);
        }
        catch {
            // Nothing is waiting on this by construction, so a dead port is not an error to
            // report — it is the ordinary end of a worker that is going away.
        }
    }
    #receive(e) {
        const envelope = e.data;
        if (!envelope?.f)
            return;
        const attachments = envelope.a ?? [];
        let decoded;
        try {
            decoded = decodeFrame(envelope.f);
        }
        catch (err) {
            // Undecodable bytes are not one caller's problem: the channel is carrying garbage
            // and every request on it is unanswerable. Failing them all reports that, where
            // dropping the message parks each one until its own deadline with no reason given.
            this.close(err);
            return;
        }
        // A request carries `call`; a reply carries `result`. Nothing else distinguishes
        // them, and nothing else needs to — a side only looks up seqs it issued itself, so
        // the two directions cannot collide however they number their own.
        if (decoded.header.call !== undefined) {
            void this.#answer(envelope.f, attachments);
            return;
        }
        const waiter = this.#inflight.get(decoded.header.seq);
        if (!waiter)
            return;
        this.#inflight.delete(decoded.header.seq);
        waiter.resolve({ decoded, attachments });
    }
    /**
     * Route one message and post its answer.
     *
     * Public because the bootstrap message arrives before there is a port to receive it on
     * — it is what carries the port — and must still be answered the same way as every
     * message after it.
     */
    async deliver(frame, attachments = []) {
        return this.#answer(frame, attachments);
    }
    async #answer(frame, attachments) {
        const out = await this.router.handle(frame, attachments);
        const port = this.#port;
        if (!port)
            return;
        const buffer = asArrayBuffer(out.frame);
        const envelope = { f: buffer };
        if (out.transfer?.length)
            envelope.a = out.transfer;
        try {
            port.postMessage(envelope, [buffer, ...(out.transfer ?? [])]);
        }
        catch {
            // The port closed between the request and the answer. Whoever asked is going away
            // too, and its own deadline covers anything still parked.
        }
    }
    /**
     * Fail everything outstanding and stop sending.
     *
     * Called on teardown and on a channel that has started delivering nonsense. Neither
     * side did this before, so a terminated worker left its own callers parked forever on
     * promises nothing would ever settle.
     */
    close(reason) {
        this.#closed =
            reason ??
                new Error("wire: the message port closed while calls were pending");
        const outstanding = [...this.#inflight.values()];
        this.#inflight.clear();
        for (const waiter of outstanding)
            waiter.reject(this.#closed);
        try {
            this.#port?.close();
        }
        catch {
            // Already gone.
        }
        this.#port = undefined;
    }
}

// Where `node:net` / `node:tls` get their TCP: ONE Wisp relay URL.
//
// Everything above the socket — `net.connect`, `tls.connect`, `http.request`,
// `fetch` in a worker with no puter token — bottoms out in epoxy, which tunnels
// TCP (and terminates TLS itself, so TLS needs no configuration of its own)
// over a Wisp websocket. So the whole of "does this worker have a network" is
// the answer to "which relay URL do we dial", and that is what this module
// decides.
//
// ORDER OF SOURCES, and why. The URL is the one thing on this path that can put
// a THIRD PARTY in front of a user's traffic, so it is resolved deliberately
// rather than defaulted:
//
//   1. `net.wispUrl` — what the caller of `NodeWorker.create` passed, dialed as
//      given (a wisp v1 URL carries its relay token in the path, so nothing is
//      parsed out of it).
//   2. `globalThis.HIVE_WISP_URL` — the platform-configured relay. The host page
//      writes it here from the operator's `HIVE_BROWSER_WISP_URL`, which
//      reaches the page in the admission capability's `net` block; a host that
//      never sets it simply does not participate.
//   3. the PUBLIC fallback relay, only when
//      `globalThis.HIVE_WISP_PUBLIC_FALLBACK === true` — the upstream README's
//      example relay, i.e. infrastructure neither this package nor the host
//      controls. Because it is a third party it is (a) off unless asked for,
//      (b) PROVEN reachable by a bounded websocket handshake before any donor
//      traffic is routed at it, and (c) named in a console warning every time
//      it is chosen — disclosed, never silently enabled.
//   4. none of the above — no URL. That is not an error at startup (a worker
//      that never opens a socket has no network problem), but the first socket
//      throws `WispRelayUnavailable` NAMING what is missing: a relay-less
//      worker must never look like one with a broken network.
//
// The one case that DOES throw here is the fallback being enabled and not
// answering: the caller asked for a network and there is now proof that the
// only source left cannot carry one.
/** The public fallback: upstream node-worker's own README example. */
const PUBLIC_WISP_FALLBACK_URL = "wss://anura.pro/";
/** How long the fallback relay gets to answer a handshake. `0` disables the probe. */
const DEFAULT_PUBLIC_FALLBACK_PROBE_MS = 5_000;
/**
 * No relay to dial, named — as opposed to an opaque socket failure.
 *
 * The `name` is the contract: a host matching `err.name === "WispRelayUnavailable"`
 * can tell "this platform has no relay configured" apart from "the relay is down"
 * and from "the program dialed a bad host", which is the difference between an
 * operator's to-do and a guest program's bug.
 */
class WispRelayUnavailable extends Error {
    constructor(message) {
        super(message);
        this.name = "WispRelayUnavailable";
    }
}
/**
 * A `ws:` or `wss:` URL with a host, and nothing that could smuggle a second
 * origin in — the string is dialed verbatim.
 */
function isWispUrl(value) {
    if (typeof value !== "string")
        return false;
    const raw = value.trim();
    if (!raw || /\s/.test(raw))
        return false;
    const rest = raw.startsWith("wss://")
        ? raw.slice("wss://".length)
        : raw.startsWith("ws://")
            ? raw.slice("ws://".length)
            : undefined;
    if (rest === undefined)
        return false;
    // Authority is everything before the first path, query or fragment
    // delimiter: past it, the string is the relay's own (a wisp v1 URL carries
    // its token in the path), before it, it has to be a host with an optional
    // numeric port.
    const authority = rest.split(/[/?#]/)[0].split("@").pop() ?? "";
    if (!authority)
        return false;
    const port = authority.split(":").pop();
    if (authority.includes(":") && !/^\d+$/.test(port ?? ""))
        return false;
    return true;
}
/** The platform-configured relay, published by the host page. See source 2. */
function platformWispUrl() {
    const value = globalThis.HIVE_WISP_URL;
    return isWispUrl(value) ? value.trim() : undefined;
}
/** The fallback relay's address: `net`'s, then the host's, then the default. */
function publicFallbackUrl(net) {
    const override = net?.publicFallbackUrl;
    if (isWispUrl(override))
        return override.trim();
    const value = globalThis
        .HIVE_WISP_PUBLIC_FALLBACK_URL;
    return isWispUrl(value) ? value.trim() : PUBLIC_WISP_FALLBACK_URL;
}
/**
 * Whether the third-party fallback may be used at all.
 *
 * Opt-in on either side: per-worker (`net.allowPublicFallback`) or fleet-wide
 * (`globalThis.HIVE_WISP_PUBLIC_FALLBACK`, written from the host's own config).
 * Absent means no — the default has to be the safe one, because the cost of
 * guessing wrong here is a third party reading someone's traffic.
 */
function publicFallbackAllowed(net) {
    return (net?.allowPublicFallback === true ||
        globalThis
            .HIVE_WISP_PUBLIC_FALLBACK === true);
}
/**
 * Prove a relay answers, by completing a websocket handshake and hanging up.
 *
 * A handshake is the only liveness proof available from a page: wisp has no
 * health endpoint every relay is obliged to serve, and a relay that upgrades is
 * a relay that can carry a stream. Bounded, because this runs before a worker
 * starts and a silent third party must not be able to stall it.
 */
async function probeWispRelay(url, timeoutMs) {
    if (timeoutMs <= 0)
        return;
    let socket;
    try {
        socket = new WebSocket(url);
    }
    catch (err) {
        throw new Error(`could not open a websocket to ${url} (${err?.message ?? err})`);
    }
    try {
        await new Promise((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error(`no handshake in ${timeoutMs}ms`)), timeoutMs);
            const settle = (err) => {
                clearTimeout(timer);
                if (err)
                    reject(err);
                else
                    resolve();
            };
            socket.onopen = () => settle();
            socket.onerror = () => settle(new Error("websocket error"));
            socket.onclose = (event) => settle(new Error(`closed before opening (code ${event.code})`));
        });
    }
    finally {
        try {
            socket.close();
        }
        catch {
            /* already gone — the probe is over either way */
        }
    }
}
/**
 * Which relay this anonymous worker dials, or `undefined` for none.
 *
 * Never called when a puter token was passed: a token mints relay credentials
 * of its own, and `net` is documented as ignored in that case.
 */
async function resolveWispRelay(net) {
    const explicit = net?.wispUrl?.trim();
    if (explicit) {
        if (!isWispUrl(explicit)) {
            throw new WispRelayUnavailable(`net.wispUrl is not a ws:// or wss:// URL: ${JSON.stringify(net?.wispUrl)}`);
        }
        return { url: explicit, source: "caller", thirdParty: false };
    }
    const platform = platformWispUrl();
    if (platform)
        return { url: platform, source: "platform", thirdParty: false };
    if (!publicFallbackAllowed(net))
        return undefined;
    const url = publicFallbackUrl(net);
    const timeoutMs = net?.publicFallbackProbeMs ?? DEFAULT_PUBLIC_FALLBACK_PROBE_MS;
    try {
        await probeWispRelay(url, timeoutMs);
    }
    catch (err) {
        throw new WispRelayUnavailable(`no wisp relay is reachable: the public fallback ${url} did not complete a websocket ` +
            `handshake in ${timeoutMs}ms (${err?.message ?? err}), and no platform relay ` +
            `is configured — node:net / node:tls are unavailable in this worker`);
    }
    // DISCLOSURE, operator-facing and unconditional: a third party is about to
    // terminate and re-emit every connection this worker makes. Saying so is the
    // whole difference between a fallback and a silent MITM.
    console.warn(`[node-worker] node:net / node:tls are routed through the THIRD-PARTY public wisp relay ` +
        `${url}: every outbound connection from this worker is terminated and re-emitted there. ` +
        `Configure a platform relay to keep this traffic on infrastructure you control.`);
    return { url, source: "public-fallback", thirdParty: true };
}

// Frame in, frame out, for process ops. The mirror of ../vfs/dispatch.ts.
//
// Both transports land here on the same bytes: the blocking `XMLHttpRequest` a worker uses for
// `spawnSync`, and the `postMessage` it uses for everything else. One implementation, so a
// framing or error-shape bug cannot exist on one and not the other.
// Part lengths are derived by `encodeReply`, not written out here. They used to be set by
// hand at each call site that had bytes to send, which is one transcription per op and one
// chance each to disagree with the payload actually attached.
function reply$1(header, parts) {
    return encodeReply(KIND_PROCESS, header, parts);
}
async function handleProcessFrame(provider, frame, replay) {
    let request;
    let parts;
    try {
        const decoded = decodeFrame(frame);
        request = decoded.header;
        parts = primaryParts(decoded.header, decoded.parts);
    }
    catch (err) {
        // No seq to echo, so answer with one anyway: a worker parked on a reply that never
        // comes is worse than a caller getting an error it can report.
        return reply$1({
            seq: 0,
            result: { ok: false, error: toWireError(err, "spawn") },
        });
    }
    const { seq, call } = request;
    /*
     * A repeat means the worker retried after a transport failure, and this is the one kind
     * where re-executing is worst: `proc.spawnSync` is sent over the blocking transport, which
     * retries the same `seq` twice more on a timeout (see `SYNC_RETRIES`). Without a record the
     * provider runs the program again — three times in all for one call, with a real shell on
     * the other end. The filesystem has had this since it had a sync transport; the process side
     * never did.
     */
    const already = replay && recall(replay.cache, replay.sid, seq);
    if (already)
        return already;
    const answer = await perform$1(provider, call, parts, seq);
    /*
     * `proc.poll` is deliberately not recorded. It is async-only by construction — the worker
     * keeps one outstanding and never sends it synchronously — so it can never be retried, and
     * its replies are the ones carrying a child's output in full. Remembering 64 of those per
     * session would hold megabytes to guard a retry that cannot happen.
     */
    if (replay && call.op !== "proc.poll") {
        remember(replay.cache, replay.sid, seq, answer);
    }
    return answer;
}
async function perform$1(provider, call, parts, seq) {
    try {
        if (!provider) {
            throw Object.assign(new Error("no process provider is registered — pass `process` to NodeWorker.create, " +
                "or call worker.registerProcessProvider()"), { code: "ENOSYS" });
        }
        switch (call.op) {
            case "proc.probe":
                return reply$1({
                    seq,
                    result: { ok: true, value: { provider: provider.name } },
                });
            case "proc.spawn":
                return reply$1({
                    seq,
                    result: {
                        ok: true,
                        value: await provider.spawn(call.ctx, call.request),
                    },
                });
            case "proc.poll": {
                const events = await provider.poll(call.ctx, call.pid);
                /*
                 * The bytes ride in the payload and the header only says what each event *is*,
                 * in the same order. A child that prints a megabyte would otherwise put that
                 * megabyte through `JSON.stringify` on the way out and `JSON.parse` on the way
                 * in, per poll.
                 */
                const bytes = [];
                const described = events.map((e) => {
                    if (e.kind === "exit") {
                        return {
                            kind: "exit",
                            status: e.status,
                            signal: e.signal,
                        };
                    }
                    bytes.push(e.bytes);
                    return { kind: e.kind };
                });
                return reply$1({ seq, result: { ok: true, value: { events: described } } }, bytes);
            }
            case "proc.write":
                await provider.write(call.ctx, call.pid, parts[0] ?? new Uint8Array(0));
                return reply$1({ seq, result: { ok: true, value: undefined } });
            case "proc.endStdin":
                await provider.endStdin(call.ctx, call.pid);
                return reply$1({ seq, result: { ok: true, value: undefined } });
            case "proc.kill":
                await provider.kill(call.ctx, call.pid, call.signal);
                return reply$1({ seq, result: { ok: true, value: undefined } });
            case "proc.spawnSync": {
                const result = await provider.spawnSync(call.ctx, {
                    ...call.request,
                    input: parts[0],
                });
                const out = [result.stdout, result.stderr];
                return reply$1({
                    seq,
                    result: {
                        ok: true,
                        value: {
                            status: result.status,
                            signal: result.signal,
                            error: result.error,
                        },
                    },
                }, out);
            }
            default: {
                const unknown = call;
                throw Object.assign(new Error(`unknown process op ${unknown.op}`), {
                    code: "ENOSYS",
                });
            }
        }
    }
    catch (err) {
        return reply$1({
            seq,
            result: { ok: false, error: toWireError(err, syscallOf(call)) },
        });
    }
}
/** The syscall name an error should carry, which is the caller's word for what it asked. */
function syscallOf(call) {
    return "ctx" in call ? call.ctx.syscall : "spawn";
}

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
/** Carries a rescue to same-origin pages regardless of control or scope. */
const SW_BROADCAST_CHANNEL = "node-worker-wire";
/**
 * ms the service worker waits for an answer before failing the request.
 *
 * Refreshed by every {@link SwProgress}, so this is a liveness deadline rather than a
 * limit on how long an operation may take. Kept well inside the browsers' own
 * fetch-event ceilings (Chrome ~5 min, Firefox ~300s) so the failure is ours and legible
 * rather than theirs and opaque.
 */
const OP_TIMEOUT_MS = 15_000;
/**
 * ms between heartbeats while an op is outstanding.
 *
 * A third of the deadline: two may be lost — to a page busy on the main thread, or to a
 * `postMessage` arriving late — without the SW concluding the page is gone.
 */
const PROGRESS_INTERVAL_MS = OP_TIMEOUT_MS / 3;
/**
 * ms the *worker* waits, as the backstop.
 *
 * Longer than `OP_TIMEOUT_MS` on purpose: the SW's own timeout produces a `504` with a
 * readable message, so it should normally win the race. This one exists for the case
 * where there is no service worker left to produce anything — and it is the only limit
 * that holds then, because nothing else in the worker can run while the thread is
 * parked. (`xhr.timeout` is settable on a synchronous request in a worker; the spec only
 * forbids it when the global is a `Window`.)
 *
 * An op that may legitimately park for as long as a person is willing to wait — a
 * blocking `io.read` at a prompt — opts out with 0, and is then bounded only by the
 * heartbeat above failing.
 */
const SYNC_TIMEOUT_MS = 20_000;

// A posix path implementation, and the mount-table string work that goes with it.
//
// The worker gets node's own `path` (src/worker/node/path.ts re-exports it out of the
// node_core tree), but the page and the service worker cannot: `dist/index.js` is
// built with typescript alone and `node-core:` specifiers only resolve inside the
// worker bundle. Since the mount table and every provider need `dirname`/`basename`
// on the host, they need these.
//
// Only what the filesystem layer actually uses is here, and every function is posix —
// there are no drive letters or backslashes in this namespace, so the win/posix split
// node carries is not reproduced.
/** The api clamps a recursive listing's depth to this, and node's readdir has no limit. */
const MAX_DEPTH = 10;
/**
 * Collapse `.`, `..` and repeated separators.
 *
 * A `..` above an absolute root is dropped rather than kept, which is the
 * containment guarantee the mount layer relies on: `normalize("/tmp/../../etc")` is
 * `/etc`, never `/../etc`, so no path can climb out of the root. A *relative* input
 * keeps its leading `..` segments, as node does.
 */
function normalize(p) {
    const absolute = p.charCodeAt(0) === 47; /* "/" */
    const trailing = p.length > 1 && p.charCodeAt(p.length - 1) === 47;
    const out = [];
    for (const segment of p.split("/")) {
        if (segment === "" || segment === ".")
            continue;
        if (segment === "..") {
            if (out.length > 0 && out[out.length - 1] !== "..")
                out.pop();
            else if (!absolute)
                out.push("..");
            // An absolute path with nothing left to pop stays at the root.
            continue;
        }
        out.push(segment);
    }
    let joined = out.join("/");
    if (absolute)
        joined = "/" + joined;
    if (joined === "")
        return absolute ? "/" : ".";
    // node keeps a trailing slash on a normalized path, and `resolve` drops it. Only
    // `normalize` preserves it, so callers that need canonical mount roots must go
    // through `resolveFrom` or `checkRoot`.
    if (trailing && joined !== "/")
        joined += "/";
    return joined;
}
/**
 * `path.resolve`, anchored explicitly.
 *
 * node's `resolve` falls back to `process.cwd()` when its accumulated result isn't
 * absolute; there is no cwd here, so the base is always a parameter. Every caller in
 * this tree passes `"/"`, which is what makes the containment guarantee above
 * unconditional.
 */
function resolveFrom(base, ...parts) {
    let acc = base;
    for (const part of parts) {
        if (!part)
            continue;
        acc =
            part.charCodeAt(0) === 47
                ? part
                : acc === "/"
                    ? "/" + part
                    : acc + "/" + part;
    }
    const normalized = normalize(acc);
    if (normalized.length > 1 && normalized.endsWith("/"))
        return normalized.slice(0, -1);
    return normalized;
}
function join(...parts) {
    const joined = parts.filter((p) => p !== "").join("/");
    return joined === "" ? "." : normalize(joined);
}
function dirname(p) {
    if (p === "/" || p === "")
        return p === "" ? "." : "/";
    // A trailing slash is not a segment: dirname("/a/b/") is "/a", as node has it.
    let end = p.length;
    while (end > 1 && p.charCodeAt(end - 1) === 47)
        end--;
    const slash = p.lastIndexOf("/", end - 1);
    if (slash === -1)
        return ".";
    if (slash === 0)
        return "/";
    return p.slice(0, slash);
}
function basename(p, ext) {
    let end = p.length;
    while (end > 1 && p.charCodeAt(end - 1) === 47)
        end--;
    const slash = p.lastIndexOf("/", end - 1);
    let base = p.slice(slash + 1, end);
    if (base === "/")
        base = "";
    return base;
}
// ------------------------------------------------------- the mount-table string work
/**
 * Whether `p` is at or below `root`.
 *
 * The segment-boundary check is load-bearing: a plain `startsWith` makes `/foo-bar` a
 * child of `/foo`. Same trap `relDepth` documents below.
 */
function under(root, p) {
    if (root === "/")
        return true;
    if (!p.startsWith(root))
        return false;
    return p.length === root.length || p.charCodeAt(root.length) === 47; /* "/" */
}
/** Re-root an absolute path onto its mount, so the provider sees it from "/". */
function toLocal(root, p) {
    if (root === "/")
        return p;
    const rest = p.slice(root.length);
    return rest === "" ? "/" : rest;
}
/**
 * Mount roots must arrive canonical; this checks rather than fixes.
 *
 * Deliberately hand-rolled string work rather than `resolveFrom`, and kept that way
 * on the move host-side. Mounts used to be registered at *module scope* inside the
 * worker's fs init cycle, where `node/path` might not exist yet — that constraint is
 * gone, but demanding a canonical root still costs internal callers nothing and turns
 * a silently misrouted mount into a loud error.
 */
function checkRoot(root) {
    if (root === "/")
        return "/";
    if (!root.startsWith("/") ||
        root.endsWith("/") ||
        root.includes("//") ||
        root.split("/").some((s) => s === "." || s === "..")) {
        throw new Error(`mount root must be absolute and canonical, got ${JSON.stringify(root)}`);
    }
    return root;
}
/**
 * How many path segments `p` sits below `root`; direct children are 1, and -1 means
 * `p` isn't under `root` at all.
 *
 * The trailing slash on the prefix is load-bearing: without it `/foo-bar` reads as a
 * descendant of `/foo`.
 */
function relDepth(root, p) {
    const prefix = root === "/" ? "/" : root + "/";
    if (!p.startsWith(prefix))
        return -1;
    let depth = 1;
    for (let i = prefix.length; i < p.length; i++) {
        if (p.charCodeAt(i) === 47 /* "/" */)
            depth++;
    }
    return depth;
}

// Watch events synthesized from a mutation this filesystem just performed.
//
// puterfs echoes every write back over its socket.io feed, so watchers would eventually
// see our own changes anyway — but only after a round trip, and only while the socket is
// up. Emitting the moment a call succeeds is what makes write-then-observe (chokidar's
// `awaitWriteFinish`, a dev server's HMR trigger) feel immediate. A memory or OPFS mount
// has no feed at all, so for those this is the *only* source of events.
//
// Passed to a provider rather than reached for as a module global, because the events
// have to arrive at the worker that caused them and the filesystem is per session. The
// transport carries them back on the reply frame to the very call that produced them —
// which preserves the timing above and, unlike a separate push, works while the worker
// is parked inside a synchronous call, because the event rides the response it is
// already waiting for.
//
// Echoes are deliberately NOT deduplicated against the socket feed. The local event is
// the fast, approximate signal (nothing on the write path stats first, so a creation may
// be reported as `updated`); the echo that follows carries the api's own classification.
// node's `fs.watch` is documented as coalescing and occasionally double-reporting, and
// every real consumer re-stats and dedupes anyway — so an extra event is cheap and a
// missing one is not.
function createFsEvents(emit) {
    return {
        write(path) {
            emit({ kind: "updated", path, isDir: false });
        },
        add(path, isDir = false) {
            emit({ kind: "added", path, isDir });
        },
        remove(path, isDir = false) {
            emit({ kind: "removed", path, isDir });
        },
        move(oldPath, path, isDir = false) {
            emit({ kind: "moved", path, oldPath, isDir });
        },
    };
}
/** For a provider built outside a session — tests, and the host reading its own mount. */
const NO_EVENTS = {
    write() { },
    add() { },
    remove() { },
    move() { },
};

// What a *provider* throws.
//
// The envelope these errors cross the boundary in — `WireError`, `toWireError`,
// `fromWireError`, the errno table and `formatFsMessage` — moved to ../wire/error.ts,
// because it stopped being the filesystem's: every kind of message reports failure
// that way now. What is left here is the vocabulary a provider writes against, which
// is genuinely filesystem-specific and which ../lib/vfs/* imports by the dozen.
/**
 * What a provider throws.
 *
 * Deliberately **branded** rather than identified by `instanceof`: a third-party
 * provider bundled separately gets its own copy of this class, and an `instanceof`
 * check would fail on it — silently downgrading its ENOENTs to EIO, which is the
 * one mistranslation that breaks every `if (e.code !== "ENOENT")` above it.
 */
class VfsError extends Error {
    /** The brand. Structural, so a separately-bundled copy is still recognized. */
    __nodeWorkerFsError = 1;
    code;
    errno;
    syscall;
    path;
    dest;
    constructor(code, opts = {}) {
        const known = ERRNO[code];
        const bare = opts.message ?? known?.message ?? "i/o error";
        super(formatFsMessage(code, bare, opts.syscall, opts.path));
        this.name = "Error";
        this.code = code;
        this.errno = known?.errno ?? ERRNO.EIO.errno;
        if (opts.syscall)
            this.syscall = opts.syscall;
        if (opts.path)
            this.path = opts.path;
        if (opts.dest)
            this.dest = opts.dest;
    }
}
/**
 * The idiom a provider uses: `throw fsError("ENOENT", ctx)`.
 *
 * Takes a {@link WireCtx}-shaped object so the common case is one argument — the
 * context the operation was already handed.
 */
function fsError(code, ctx) {
    return new VfsError(code, {
        syscall: ctx?.syscall,
        path: ctx?.path ?? ctx?.reportPath,
        dest: ctx?.dest,
        message: ctx?.message,
    });
}

// Turning bytes already in hand into a `ProviderStream`.
//
// Used in two places, and it matters that it is the same code in both: the facade
// synthesizes a stream for any provider without `openRead`, and `unionProvider` needs it
// for the case where the layer it picked cannot stream but the other one can. Two copies
// would be two chances to get the `start`/`end` window subtly different, and `end` being
// *inclusive* (node's `createReadStream` contract) is exactly the kind of off-by-one that
// survives review.
/**
 * `range.end` is **inclusive**, as node's `createReadStream` has it, and both ends are
 * clamped so a window past EOF yields an empty stream rather than an error — which is what
 * node does for a `start` at or beyond the end of the file.
 */
function streamOfBytes(bytes, range) {
    const start = Math.min(Math.max(range?.start ?? 0, 0), bytes.length);
    const end = range?.end === undefined
        ? bytes.length
        : Math.min(Math.max(range.end + 1, start), bytes.length);
    // A copy, not a view: the consumer owns what comes out of a stream, and this one is
    // handed across a worker boundary where the backing buffer may be transferred.
    const slice = new Uint8Array(bytes.subarray(start, end));
    return {
        size: slice.length,
        stream: new ReadableStream({
            start(controller) {
                if (slice.length > 0)
                    controller.enqueue(slice);
                controller.close();
            },
        }),
    };
}

// The filesystem facade: what everything above this layer calls.
//
// Resolves a path to its mount, delegates, and owns the three things that are about the
// *namespace* rather than any one backend:
//
//   - **Mount grafting.** A listing has to show mounts rooted beneath it, and a recursive
//     listing must not report the real contents of a directory that a mount shadows.
//   - **Cross-mount operations.** No single backend can rename between two of them.
//   - **The optional half of the provider interface.** `readRange`, `copyFile` and
//     `openRead` are fast paths where a backend has one and are derived here where it
//     doesn't, so no caller ever branches on provider capability.
//
// Deliberately NOT here: argument coercion and node's classes, which stay in the worker
// with the `fs` surface, and anything that reads the cwd. Every path reaching this file is
// already absolute and normalized — the worker resolves against its own `process.cwd()`
// before the call crosses, which is also the containment guarantee the mount layer relies
// on.
/** What `statfs` reports for a backend that has no notion of capacity. See `statfs` below. */
const UNKNOWN_CAPACITY = 1024 ** 3;
/** Lift a provider's mount-local entry onto the absolute namespace. */
function reroot(root, entry) {
    if (root === "/")
        return entry;
    return { ...entry, path: entry.path === "/" ? root : root + entry.path };
}
/** The directory entry a mount point presents to a listing of its parent. */
function mountPointEntry(m) {
    return {
        path: m.root,
        name: basename(m.root),
        uid: "",
        isDir: true,
        isSymlink: false,
        size: 0,
        modifiedMs: m.createdMs,
        createdMs: m.createdMs,
        accessedMs: m.createdMs,
    };
}
class Facade {
    #table;
    /**
     * Operations per mount root, for the same reason the api call counts exist: this is the
     * only way to see how much filesystem traffic a run actually makes, and after the move
     * every one of these is a round trip the worker paid for.
     */
    #opCounts = new Map();
    constructor(table) {
        this.#table = table;
    }
    opStats() {
        return Object.fromEntries([...this.#opCounts].sort((a, b) => b[1] - a[1]));
    }
    resetOpStats() {
        this.#opCounts.clear();
    }
    #resolve(path, ctx) {
        const r = this.#table.resolve(path);
        const key = `${r.mount.root} ${ctx.syscall}`;
        this.#opCounts.set(key, (this.#opCounts.get(key) ?? 0) + 1);
        return r;
    }
    #assertWritable(m, ctx) {
        if (m.readOnly)
            throw fsError("EROFS", ctx);
    }
    // ------------------------------------------------------------- primitives
    async stat(ctx, path) {
        const r = this.#resolve(path, ctx);
        return reroot(r.mount.root, await r.mount.provider.stat(ctx, r.local));
    }
    async readdir(ctx, path, opts) {
        const r = this.#resolve(path, ctx);
        const listing = await r.mount.provider.readdir(ctx, r.local, opts);
        let entries = listing.entries.map((e) => reroot(r.mount.root, e));
        let complete = listing.complete;
        if (opts?.recursive) {
            // The provider happily listed the real contents of a directory that a mount
            // shadows. Drop them; the recursion below contributes the mount's own view.
            entries = entries.filter((e) => !this.#shadowed(path, e.path));
        }
        // Graft in mounts rooted directly beneath this directory. Never stats them: a stat
        // per mount per listing is a hidden round trip on a hot path, and a caller that
        // wants real numbers stats the path, which resolves *into* the mount and gets the
        // truth.
        for (const m of this.#table.childMounts(path)) {
            const name = basename(m.root);
            const i = entries.findIndex((e) => e.name === name);
            // A mount over a real directory keeps that directory's timestamps, the way a
            // mount point reports the underlying dentry on Linux.
            if (i >= 0)
                entries[i] = { ...entries[i], isDir: true, isSymlink: false };
            else
                entries.push(mountPointEntry(m));
        }
        if (opts?.recursive) {
            for (const m of this.#table.mountsUnder(path)) {
                const sub = await this.readdir(ctx, m.root, opts);
                entries.push(...sub.entries);
                complete = complete && sub.complete;
            }
        }
        return { entries, complete };
    }
    async readFile(ctx, path) {
        const r = this.#resolve(path, ctx);
        return r.mount.provider.readFile(ctx, r.local);
    }
    async writeFile(ctx, path, data) {
        const r = this.#resolve(path, ctx);
        this.#assertWritable(r.mount, ctx);
        await r.mount.provider.writeFile(ctx, r.local, data);
    }
    async mkdir(ctx, path, opts) {
        const r = this.#resolve(path, ctx);
        this.#assertWritable(r.mount, ctx);
        // node reports the first directory a recursive mkdir created; the provider names it
        // in its own local terms, so lift it back onto the mount.
        const first = await r.mount.provider.mkdir(ctx, r.local, opts);
        if (first === undefined)
            return undefined;
        return r.mount.root === "/" ? first : r.mount.root + first;
    }
    async rm(ctx, path, opts) {
        const r = this.#resolve(path, ctx);
        this.#assertWritable(r.mount, ctx);
        await r.mount.provider.rm(ctx, r.local, opts);
    }
    async rename(ctx, from, to) {
        const src = this.#resolve(from, ctx);
        const dst = this.#resolve(to, ctx);
        this.#assertWritable(src.mount, ctx);
        this.#assertWritable(dst.mount, ctx);
        if (src.mount === dst.mount) {
            await src.mount.provider.rename(ctx, src.local, dst.local);
            return;
        }
        // No backend can move bytes into another one, so this degrades to copy-then-delete.
        // Deliberately transparent rather than EXDEV: vite and npm both rename a temp file
        // into place, and those two paths will routinely straddle a mount boundary once
        // node_modules is served from an archive. The cost is that it is not atomic, which
        // is worth stating out loud.
        const entry = await src.mount.provider.stat(ctx, src.local);
        if (entry.isDir)
            throw fsError("EXDEV", { ...ctx, path: from });
        const data = await src.mount.provider.readFile(ctx, src.local);
        await dst.mount.provider.writeFile(ctx, dst.local, data);
        await src.mount.provider.rm(ctx, src.local, {
            recursive: false,
            force: false,
        });
    }
    async utimes(ctx, path, atimeMs, mtimeMs) {
        const r = this.#resolve(path, ctx);
        this.#assertWritable(r.mount, ctx);
        return r.mount.provider.utimes(ctx, r.local, atimeMs, mtimeMs);
    }
    // --------------------------------------------- always available, sometimes derived
    /** Sliced out of a whole-file read when the backend has no positioned read. */
    async readRange(ctx, path, offset, length) {
        const r = this.#resolve(path, ctx);
        const p = r.mount.provider;
        if (p.readRange)
            return p.readRange(ctx, r.local, offset, length);
        const whole = await p.readFile(ctx, r.local);
        return whole.subarray(offset, offset + length);
    }
    /** Read-then-write when the backend has no server-side copy. */
    async copyFile(ctx, from, to, opts) {
        const src = this.#resolve(from, ctx);
        const dst = this.#resolve(to, ctx);
        this.#assertWritable(dst.mount, ctx);
        // The server-side copy is only usable when both ends are the same backend.
        if (src.mount === dst.mount && src.mount.provider.copyFile) {
            await src.mount.provider.copyFile(ctx, src.local, dst.local, opts);
            return;
        }
        if (!opts.overwrite) {
            let exists = true;
            try {
                await dst.mount.provider.stat({ syscall: "stat", reportPath: to }, dst.local);
            }
            catch (err) {
                const code = err.code;
                if (code !== "ENOENT" && code !== "ENOTDIR")
                    throw err;
                exists = false;
            }
            if (exists)
                throw fsError("EEXIST", { ...ctx, path: to });
        }
        const data = await src.mount.provider.readFile(ctx, src.local);
        await dst.mount.provider.writeFile(ctx, dst.local, data);
    }
    /**
     * Always available: synthesized from a whole-file read when the backend cannot stream.
     *
     * This is why the worker no longer has to ask whether a path is streamable — the
     * question that `createReadStream` used to get wrong by assuming every path was served
     * by puterfs. `MountSnapshot.canStream` survives only as a hint about whether the stream
     * is *native*, i.e. whether it avoids buffering the file first.
     */
    async openRead(ctx, path, range) {
        const r = this.#resolve(path, ctx);
        const p = r.mount.provider;
        if (p.openRead)
            return p.openRead(ctx, r.local, range);
        return streamOfBytes(await p.readFile(ctx, r.local), range);
    }
    async statfs(ctx, path) {
        const r = this.#resolve(path, ctx);
        const p = r.mount.provider;
        // A backend with no notion of capacity still must not look *full*. Zeros here were read as
        // "no bytes free" by anything that checks for room before writing — which is how a memory
        // `/tmp` with plenty of space failed every Claude Code Bash command. Report a plausible
        // capacity that is entirely free instead: unknown is closer to "room available" than to
        // "none", and the honest alternative — refusing to answer — is not open to us, because
        // node's `statfs` has no way to say "I don't know".
        if (!p.statfs)
            return { used: 0, capacity: UNKNOWN_CAPACITY };
        return p.statfs(ctx);
    }
    /** Whether `p` lies inside a mount grafted somewhere beneath `dir`. */
    #shadowed(dir, p) {
        return this.#table.mountsUnder(dir).some((m) => under(m.root, p));
    }
}

// The mount table: which backend serves which subtree.
//
// Deliberately flat — one provider per root, longest matching prefix wins. Layering two
// backends over the same subtree is a *provider* concern (./union.ts), not a table
// concern, which keeps the lookup trivially correct and the layering independently
// testable.
//
// A leaf module: it knows nothing about any concrete provider, so providers can depend
// on the table's types without the table depending on them.
//
// One table per session, not one per page. Each `NodeWorker` has had its own `/tmp`, its
// own overlay and its own memory mounts for as long as those have existed — a single
// shared namespace would silently start sharing all three, which is wrong for injected
// modules and for a consumer that mounts a project per worker and treats each as a
// private replica.
class MountTable {
    /**
     * Sorted by root length descending, so the first match is the longest one. A linear
     * scan over a handful of mounts beats any cleverer structure.
     */
    #mounts = [];
    #onChange;
    /** Notified whenever the table changes, so the worker's snapshot can be re-pushed. */
    onChange(fn) {
        this.#onChange = fn;
    }
    mount(root, provider, opts = {}) {
        const checked = checkRoot(root);
        if (this.#mounts.some((m) => m.root === checked)) {
            throw new Error(`already mounted: ${checked}`);
        }
        const entry = {
            root: checked,
            provider,
            readOnly: !!opts.readOnly,
            createdMs: Date.now(),
        };
        this.#mounts.push(entry);
        this.#mounts.sort((a, b) => b.root.length - a.root.length);
        this.#onChange?.();
        return entry;
    }
    unmount(root) {
        const checked = checkRoot(root);
        if (checked === "/")
            throw new Error("cannot unmount /");
        const before = this.#mounts.length;
        this.#mounts = this.#mounts.filter((m) => m.root !== checked);
        const changed = this.#mounts.length !== before;
        if (changed)
            this.#onChange?.();
        return changed;
    }
    resolve(path) {
        for (const mount of this.#mounts) {
            if (under(mount.root, path)) {
                return { mount, local: toLocal(mount.root, path), full: path };
            }
        }
        // Unreachable in practice: "/" is mounted at construction and matches everything.
        throw new Error(`no mount serves ${path}`);
    }
    /** Mounts rooted *directly* beneath `dir` — the ones a listing of `dir` must show. */
    childMounts(dir) {
        return this.#mounts.filter((m) => m.root !== "/" && dirname(m.root) === dir);
    }
    /** Mounts rooted strictly below `dir`, at any depth — for a recursive listing. */
    mountsUnder(dir) {
        return this.#mounts.filter((m) => m.root !== dir && under(dir, m.root));
    }
    /**
     * Whether `path` is a mount point, or an ancestor of one.
     *
     * A caching layer needs this before it may answer ENOENT from a listing: a
     * directory's children as reported by *one* provider do not include the mounts
     * grafted beneath it, so "absent from the listing" is not "absent from the
     * filesystem" for these paths.
     */
    isMountPathOrAncestor(path) {
        return this.#mounts.some((m) => m.root !== "/" && (m.root === path || under(path, m.root)));
    }
    list() {
        return this.#mounts;
    }
    /**
     * What the worker is told.
     *
     * The capability flags come from which optional methods a provider actually
     * implements, so they cannot drift from the truth — and `hasNativeRange` has to be
     * honest in both directions. Over-claiming is quadratic: a derived ranged read slices
     * a whole-file read, so a positioned-read loop over a "native" range that isn't one
     * re-reads the entire file per chunk. Under-claiming merely buffers it once.
     */
    snapshot() {
        return this.#mounts.map((m) => ({
            root: m.root,
            name: m.provider.name,
            readOnly: m.readOnly,
            createdMs: m.createdMs,
            hasNativeRange: !!m.provider.readRange,
            canStream: !!m.provider.openRead,
            hasCopyFile: !!m.provider.copyFile,
            hasStatfs: !!m.provider.statfs,
        }));
    }
}

// A read cache in front of a provider, invalidated from outside.
//
// The resolver has had a cache like this since the beginning
// (worker/module/resolve.ts), and its docblock names the exact reason it was
// never allowed out of that module: serving `fs.statSync` generally from a cache
// "would go stale the moment another puter app writes to a path, and we have no
// way to hear about that". We do now — ../fsevents.ts speaks the socket feed
// puterfs already publishes, and falls back to the change counter when the
// socket cannot authenticate. So this is that cache, generalized: one store
// serving every operation, with invalidation arriving from outside instead of
// being something each caller has to remember.
//
// ## A tree, not four maps
//
// The resolver kept `statCache`, `readFileCache`, `completeDirs` and a set of
// negatives, all keyed by path. Four flat maps mean a subtree invalidation —
// which is what "a directory was removed" and "a directory was renamed" both
// are — costs a linear sweep of everything cached. Here they are one tree, the
// way memory.ts is a tree, so dropping a subtree is dropping a node.
//
// The four collapse cleanly: a node's `entry` is the stat, a directory's `depth`
// is how far its `children` are known to be exhaustive, a file's `bytes` are the
// contents, and a `missing` node is a negative. `depth` is the load-bearing one
// — it is what turns a miss under a fully-listed directory into a local ENOENT,
// which is where most of the traffic in a module resolve or a `find` actually
// goes.
//
// ## Two kinds of invalidation, and why the coarse one is not a flush
//
// `applyEvent` is the precise kind: a path, a kind, and often a uid. It drops
// exactly what changed.
//
// `markStale` is the coarse kind — "something under this user changed and you
// were not told what", which is all the change-counter fallback can say. The
// obvious response is to throw everything away, and it is the wrong one: the
// counter is bumped by *our own* writes too, and nothing distinguishes ours from
// anyone else's, so a flush would mean every write emptied the cache. For an
// agent that edits a file and then greps the tree, that is the whole workload.
//
// So a stale mark bumps a generation instead. Nothing is discarded; everything
// merely stops being *believed*. The next read of an unbelieved path re-lists its
// parent directory — one request — and every child whose identity is unchanged is
// believed again with its bytes intact. A grep after a one-file edit costs one
// listing per directory it walks, not a re-download of the tree.
//
// Identity is `(uid, size, modifiedMs)`, and puterfs timestamps have one-second
// resolution — so two writes inside the same second, to the same length, are
// indistinguishable here. The socket path does not have this problem because it
// names the path that changed. It is the price of the fallback, and the same one
// `handles.ts` already documents for its own buffer revalidation.
//
// ## What this deliberately does not do
//
// It does not know about the mount table, per the provider invariants: the facade
// resolves a path to its mount before a provider is ever called, so a negative
// derived here can never contradict a mount grafted somewhere below — those paths
// are answered by that mount and never reach this file at all.
/** Bytes of file content held, across all files. */
const DEFAULT_MAX_BYTES = 64 * 1024 * 1024;
/**
 * Largest single file whose contents are kept.
 *
 * A cap per file as well as in total, because without it one large read evicts
 * everything else to hold something nothing is likely to read twice — and the
 * files an agent reads repeatedly are source files, which are small.
 */
const DEFAULT_MAX_FILE_BYTES = 8 * 1024 * 1024;
/**
 * How far behind the feed this cache will let itself be before it stops
 * answering from itself.
 *
 * Only reachable when the socket is down, since a live socket reports
 * `freshAsOf()` as *now*. Comfortably above the poll interval in ../fsevents.ts
 * so the steady state costs no extra request: the periodic poll keeps this
 * satisfied on its own, and `ensureFresh` only fires when a poll has failed or
 * has not happened yet.
 */
const DEFAULT_MAX_STALE_MS = 3000;
/** `statfs` is a number about the whole account, which no `item.*` event reports. */
const STATFS_TTL_MS = 3000;
/** `depth` for a listing that ran to the bottom of the tree. */
const UNBOUNDED = Infinity;
/**
 * How far a seeded listing reaches.
 *
 * Counted from the *parent* of the directory that missed — see `seedSubtree` — so 4 settles
 * that directory and three more levels below it. A walk then re-seeds every `depth - 1` levels,
 * which is what makes the cost of a walk the number of directories sitting at those *stride*
 * levels, and why this is not monotonic: a deeper seed strides further but its next frontier
 * lands on a deeper, more numerous level. Measured over five random trees of each shape, walked
 * to the bottom, counting requests to the backend:
 *
 *  | depth | ≤4 levels | ≤6 levels | ≤9 levels |
 *  |-------|-----------|-----------|-----------|
 *  | off   |       232 |       618 |       445 |
 *  | 2     |        91 |       252 |       231 |
 *  | 3     |        31 |        86 |       148 |
 *  | **4** |    **59** |    **38** |    **52** |
 *  | 5     |         9 |        70 |       106 |
 *  | 6     |         9 |       138 |        30 |
 *
 * 4 is the only value that is close to the best on every shape rather than excellent on one and
 * mediocre on the next, and it moves the fewest extra entries of the three that come close
 * (+7% to +32% over the minimum, against a 4×–16× cut in requests). Nothing here is a claim
 * about a particular tree, unlike the resolver's `SEED_DEPTH`, which is derived from the layout
 * of node_modules — and getting it wrong costs only speed.
 */
const DEFAULT_PREFETCH_DEPTH = 4;
/**
 * Shallower than this is not seeding.
 *
 * A depth-1 listing of the parent re-asks the exact question that was already answered — the
 * parent's own listing is why we know this is a descent at all — so it settles nothing and the
 * walk still pays per directory. Measured, it is *worse* than not seeding: 161 requests against
 * 121, one wasted round trip per directory.
 */
const MIN_PREFETCH_DEPTH = 2;
/**
 * Ceiling on one seeded listing. Overrunning it is not an error — the listing is a valid prefix
 * and `ingest` refuses to record a depth for it — so this only bounds what one guess about a
 * walk can cost. Same number the resolver uses for the same reason.
 */
const DEFAULT_PREFETCH_MAX_ENTRIES = 20000;
function segments$2(path) {
    return path.split("/").filter(Boolean);
}
function nameOf(path) {
    const segs = segments$2(path);
    return segs.length === 0 ? undefined : segs[segs.length - 1];
}
function childPath(dir, name) {
    return dir === "/" ? `/${name}` : `${dir}/${name}`;
}
/** Whether two entries describe the same file, unchanged. */
function sameFile(a, b) {
    return (!!a &&
        a.uid === b.uid &&
        a.size === b.size &&
        a.modifiedMs === b.modifiedMs &&
        a.isDir === b.isDir);
}
function missingCode(err) {
    const code = err?.code;
    return code === "ENOENT" || code === "ENOTDIR" ? code : undefined;
}
/**
 * The wrapper, when it is turned off.
 *
 * `enabled: false` still has to produce something with the control methods on
 * it, so the wiring above does not have to branch — and it must not simply hand
 * back `inner`, since attaching no-ops to a provider someone else also holds
 * would give them methods they never asked for. A pass-through mirrors the
 * optional halves for the same reason the real one does: the mount snapshot
 * reads capability off which of them exist.
 */
function passthrough(inner) {
    const provider = {
        name: inner.name,
        stat: (ctx, path) => inner.stat(ctx, path),
        readdir: (ctx, path, o) => inner.readdir(ctx, path, o),
        readFile: (ctx, path) => inner.readFile(ctx, path),
        writeFile: (ctx, path, data) => inner.writeFile(ctx, path, data),
        mkdir: (ctx, path, o) => inner.mkdir(ctx, path, o),
        rm: (ctx, path, o) => inner.rm(ctx, path, o),
        rename: (ctx, from, to) => inner.rename(ctx, from, to),
        utimes: (ctx, path, a, m) => inner.utimes(ctx, path, a, m),
        applyEvent: () => { },
        markStale: () => { },
        flush: () => { },
        stats: () => ({}),
    };
    if (inner.readRange) {
        provider.readRange = (ctx, p, off, len) => inner.readRange(ctx, p, off, len);
    }
    if (inner.openRead) {
        provider.openRead = (ctx, p, range) => inner.openRead(ctx, p, range);
    }
    if (inner.copyFile) {
        provider.copyFile = (ctx, from, to, o) => inner.copyFile(ctx, from, to, o);
    }
    if (inner.statfs)
        provider.statfs = (ctx) => inner.statfs(ctx);
    return provider;
}
function createCachingProvider(inner, opts = {}) {
    if (opts.enabled === false)
        return passthrough(inner);
    const maxBytes = opts.maxBytes ?? DEFAULT_MAX_BYTES;
    const maxFileBytes = opts.maxFileBytes ?? DEFAULT_MAX_FILE_BYTES;
    const maxStaleMs = opts.maxStaleMs ?? DEFAULT_MAX_STALE_MS;
    const prefix = opts.prefix ? opts.prefix : "/";
    const freshness = opts.freshness;
    const prefetch = !opts.prefetch
        ? undefined
        : {
            depth: Math.max(MIN_PREFETCH_DEPTH, (opts.prefetch === true ? undefined : opts.prefetch.depth) ??
                DEFAULT_PREFETCH_DEPTH),
            maxEntries: (opts.prefetch === true ? undefined : opts.prefetch.maxEntries) ??
                DEFAULT_PREFETCH_MAX_ENTRIES,
        };
    const counts = new Map();
    const bump = (key) => counts.set(key, (counts.get(key) ?? 0) + 1);
    let gen = 1;
    let root = { kind: "dir", children: new Map(), depth: 0, gen };
    /**
     * Files holding bytes, least-recently-used first — a `Set` because insertion
     * order *is* the ordering, and re-inserting is how a hit moves to the back.
     */
    let lru = new Set();
    let held = 0;
    /**
     * uid → the path we have it cached at.
     *
     * The api answers `POST /rename` with `item.updated` naming only the *new*
     * path — no old one — so a rename performed by another client would otherwise
     * leave a live entry at a name that no longer exists. The uid is the only
     * thing tying the two together.
     */
    let byUid = new Map();
    let statfsAt = 0;
    let statfsValue;
    /** In-flight reads, so concurrent identical ones cost one request. */
    const inflight = new Map();
    function once(key, run) {
        const existing = inflight.get(key);
        if (existing) {
            bump("coalesced");
            return existing;
        }
        const started = run().finally(() => inflight.delete(key));
        inflight.set(key, started);
        return started;
    }
    function walk(path) {
        let current = root;
        for (const seg of segments$2(path)) {
            if (current.kind === "file")
                return { blockedBy: current };
            // A missing ancestor settles everything below it.
            if (current.kind === "missing")
                return { node: current };
            const next = current.children.get(seg);
            if (!next)
                return { missingUnder: current };
            current = next;
        }
        return { node: current };
    }
    function dirNodeAt(path) {
        const node = walk(path).node;
        return node?.kind === "dir" ? node : undefined;
    }
    function live(node) {
        return node.gen === gen;
    }
    /**
     * The directory node at `path`, creating the chain if it isn't there.
     *
     * Creating a child inside a directory clears that directory's completeness:
     * it is a child the listing did not report, so whatever the listing claimed
     * about the child set is no longer what the tree holds.
     */
    function dirAt(path) {
        let current = root;
        for (const seg of segments$2(path)) {
            const next = current.children.get(seg);
            if (next?.kind === "dir") {
                current = next;
                continue;
            }
            if (next)
                forget(next);
            const created = {
                kind: "dir",
                children: new Map(),
                depth: 0,
                gen,
            };
            current.children.set(seg, created);
            current.depth = 0;
            current = created;
        }
        return current;
    }
    // ------------------------------------------------------ accounting and drops
    function release(node) {
        if (node.bytes) {
            held -= node.bytes.length;
            node.bytes = undefined;
        }
        lru.delete(node);
    }
    /** Un-account a node and everything below it, before it leaves the tree. */
    function forget(node) {
        if (node.kind === "missing")
            return;
        if (node.entry?.uid)
            byUid.delete(node.entry.uid);
        if (node.kind === "file") {
            release(node);
            return;
        }
        for (const child of node.children.values())
            forget(child);
    }
    function evict() {
        // Deleting the current element of a `Set` mid-iteration is safe; the
        // entries not yet reached are unaffected.
        for (const node of lru) {
            if (held <= maxBytes)
                return;
            bump("evicted");
            release(node);
        }
    }
    function admit(node, bytes) {
        if (maxBytes <= 0 || bytes.length > maxFileBytes)
            return;
        release(node);
        node.bytes = bytes;
        held += bytes.length;
        lru.add(node);
        evict();
    }
    function touch(node) {
        // Re-inserting moves it to the back, which is the whole ordering trick.
        if (lru.delete(node))
            lru.add(node);
    }
    // ------------------------------------------------------------ tree mutation
    /**
     * Stop believing every ancestor of `path` is completely listed.
     *
     * A path appearing or disappearing changes their child sets, and only a fresh
     * listing can say how.
     *
     * A seed is a completeness claim like any other, so it goes too: a subtree
     * whose listing has been voided is one a later walk should be free to
     * re-establish in a single request rather than a directory at a time.
     */
    function unseal(path) {
        let current = root;
        current.depth = 0;
        current.seededGen = undefined;
        for (const seg of segments$2(dirname(path))) {
            const next = current.children.get(seg);
            if (next?.kind !== "dir")
                return;
            next.depth = 0;
            next.seededGen = undefined;
            current = next;
        }
    }
    /** Put something else at `path`, or nothing. Ancestor completeness untouched. */
    function replace(path, replacement) {
        const name = nameOf(path);
        if (name === undefined) {
            flushTree();
            return;
        }
        const parentPath = dirname(path);
        const parent = replacement ? dirAt(parentPath) : dirNodeAt(parentPath);
        if (!parent)
            return;
        const existing = parent.children.get(name);
        if (existing)
            forget(existing);
        if (!replacement) {
            parent.children.delete(name);
            return;
        }
        parent.children.set(name, replacement);
        // `forget` above dropped the uid index entry, including for an identity the
        // replacement is carrying over from what it displaced.
        if (replacement.kind !== "missing" && replacement.entry?.uid) {
            byUid.set(replacement.entry.uid, path);
        }
    }
    /** Drop what we know about `path`, and what its ancestors claimed to know. */
    function invalidate(path, replacement) {
        unseal(path);
        replace(path, replacement);
    }
    /** Everything below `path` goes; `path` itself stays if it is a directory. */
    function invalidateChildren(path) {
        const dir = dirNodeAt(path);
        if (!dir) {
            invalidate(path);
            return;
        }
        for (const child of dir.children.values())
            forget(child);
        dir.children.clear();
        dir.depth = 0;
        dir.seededGen = undefined;
    }
    function flushTree() {
        root = { kind: "dir", children: new Map(), depth: 0, gen };
        lru = new Set();
        held = 0;
        byUid = new Map();
        statfsValue = undefined;
    }
    /**
     * Record how far a directory's children are known, at the current generation.
     *
     * A depth recorded before a stale mark does not survive it: the whole point of
     * a generation bump is that no completeness claim is believed until something
     * re-establishes it, and `Math.max` against the old value would quietly
     * resurrect a claim nothing verified.
     */
    function seal(dir, depth) {
        if (dir.gen !== gen) {
            dir.depth = 0;
            dir.gen = gen;
        }
        dir.depth = Math.max(dir.depth, depth);
    }
    /**
     * Record one entry, reusing the node already there when it describes the same
     * file — which is what lets a revalidating listing keep the bytes it already
     * holds instead of re-reading every file to prove nothing changed.
     */
    function put(path, entry) {
        const name = nameOf(path);
        if (name === undefined) {
            // The provider's own root. It has no parent to be a child of.
            root.entry = entry;
            root.gen = gen;
            return root;
        }
        const parent = dirAt(dirname(path));
        const existing = parent.children.get(name);
        if (entry.isDir) {
            if (existing?.kind === "dir") {
                // Crossing a generation expires what it claimed about its children.
                if (existing.gen !== gen)
                    existing.depth = 0;
                existing.entry = entry;
                existing.gen = gen;
                if (entry.uid)
                    byUid.set(entry.uid, path);
                return existing;
            }
            if (existing)
                forget(existing);
            const node = {
                kind: "dir",
                entry,
                children: new Map(),
                depth: 0,
                gen,
            };
            parent.children.set(name, node);
            if (entry.uid)
                byUid.set(entry.uid, path);
            return node;
        }
        if (existing?.kind === "file") {
            if (sameFile(existing.entry, entry))
                touch(existing);
            else
                release(existing);
            existing.entry = entry;
            existing.gen = gen;
            if (entry.uid)
                byUid.set(entry.uid, path);
            return existing;
        }
        if (existing)
            forget(existing);
        const node = { kind: "file", entry, gen };
        parent.children.set(name, node);
        if (entry.uid)
            byUid.set(entry.uid, path);
        return node;
    }
    /**
     * Record that a path is not there.
     *
     * ENOENT only. ENOTDIR says an *ancestor* is a file without saying which, and
     * writing a negative at the leaf would make `replace` build a directory chain
     * through that file to hold it — replacing a real cached file with an empty
     * directory, which is worse than knowing nothing. The derivation in `believe`
     * still answers ENOTDIR from a file node it can actually see.
     */
    function putMissing(path, code) {
        if (code !== "ENOENT")
            return;
        if (nameOf(path) === undefined)
            return;
        replace(path, { kind: "missing", code, gen });
    }
    /**
     * Fold a listing into the tree.
     *
     * `depth` may only be recorded when the listing ran to completion: a walk cut
     * short by `maxEntries` is a valid prefix but not an exhaustive picture, and
     * every negative answer this cache gives rests on the difference.
     *
     * The horizon rule is the subtle half, and it is the same one the resolver's
     * `ingestListing` documents: a directory sitting *at* the requested depth came
     * back, so it exists, but its own children were never asked for. Recording it
     * as complete would invent ENOENTs for files that are really there.
     */
    function ingest(path, depth, listing) {
        const seen = new Set();
        for (const entry of listing.entries) {
            put(entry.path, entry);
            seen.add(entry.path);
        }
        if (!listing.complete)
            return;
        const dir = dirAt(path);
        seal(dir, depth);
        reconcile(dir, path, seen);
        for (const entry of listing.entries) {
            if (!entry.isDir)
                continue;
            const below = depth - relDepth(path, entry.path);
            if (below <= 0)
                continue;
            const node = dirNodeAt(entry.path);
            if (!node)
                continue;
            seal(node, below);
            reconcile(node, entry.path, seen);
        }
    }
    /**
     * Drop children the listing did not mention.
     *
     * This is what heals a rename performed elsewhere even when no event named the
     * old path: the first fresh listing of the directory simply does not contain it
     * any more.
     */
    function reconcile(dir, dirPath, seen) {
        for (const [name, child] of [...dir.children]) {
            if (seen.has(childPath(dirPath, name)))
                continue;
            forget(child);
            dir.children.set(name, { kind: "missing", code: "ENOENT", gen });
        }
    }
    // -------------------------------------------------------------- revalidation
    /**
     * Bring the node at `path` back into the current generation, cheaply.
     *
     * One non-recursive listing of the parent re-verifies every sibling at once,
     * which is the point: after a coarse stale mark, a walk over a directory pays a
     * single request rather than one per file. Only worth doing when the parent was
     * listed before — otherwise there is no sibling set to amortize over and the
     * caller's own point read is cheaper.
     *
     * Best-effort throughout: every failure here has a correct fallback one line
     * later, which is the caller going to the backend itself.
     */
    async function revalidate(ctx, path) {
        const parentPath = dirname(path);
        if (parentPath === path)
            return;
        const parent = dirNodeAt(parentPath);
        if (!parent || parent.depth === 0)
            return;
        bump("revalidated");
        try {
            await listThrough(ctx, parentPath, 1);
        }
        catch (err) {
            if (missingCode(err))
                invalidate(parentPath);
        }
    }
    /** Whether the cache may answer from itself at all right now. */
    async function believable() {
        if (!freshness)
            return true;
        await freshness.ensureFresh(maxStaleMs);
        return Date.now() - freshness.freshAsOf() <= maxStaleMs;
    }
    /**
     * The node at `path`, in the current generation, without going to the backend.
     *
     * Undefined means the tree has nothing believable to say, which is the signal
     * to fetch.
     */
    async function believe(ctx, path, 
    /**
     * Whether an unbelieved node is worth a listing of its parent.
     *
     * False for `readdir`, where it is strictly a loss: a listing of the parent
     * re-verifies this directory's *entry* but says nothing about its children,
     * so the caller lists it anyway and has paid twice.
     */
    mayRevalidate = true) {
        if (!(await believable()))
            return undefined;
        let found = walk(path);
        if (found.node && !live(found.node) && mayRevalidate) {
            await revalidate(ctx, path);
            found = walk(path);
        }
        if (found.node)
            return live(found.node) ? found.node : undefined;
        // Not in the tree at all. An ancestor may still prove it cannot be.
        if (found.blockedBy && live(found.blockedBy)) {
            return { kind: "missing", code: "ENOTDIR", gen };
        }
        const parent = found.missingUnder;
        if (parent && live(parent) && parent.depth >= 1) {
            return { kind: "missing", code: "ENOENT", gen };
        }
        return undefined;
    }
    // ------------------------------------------------------------- backend reads
    //
    // Each of these coalesces concurrent identical requests, and each re-raises a
    // "not there" answer against *this* caller's context rather than passing the
    // shared error along: a coalesced request would otherwise report the path
    // whoever arrived first had asked about, and `WireCtx.reportPath` exists
    // precisely so an error names the path the caller spelled.
    async function statThrough(ctx, path) {
        try {
            return await once(`stat\0${path}`, async () => {
                try {
                    const entry = await inner.stat(ctx, path);
                    put(path, entry);
                    return entry;
                }
                catch (err) {
                    const code = missingCode(err);
                    if (code)
                        putMissing(path, code);
                    throw err;
                }
            });
        }
        catch (err) {
            const code = missingCode(err);
            throw code ? fsError(code, ctx) : err;
        }
    }
    async function listThrough(ctx, path, depth, readOpts) {
        // The budget is part of the key: two callers asking the same directory with
        // different `maxEntries` are asking different questions, and handing the
        // larger one a listing truncated for the smaller would be a short answer
        // reported as a whole one.
        const budget = readOpts?.maxEntries ?? "";
        try {
            return await once(`readdir\0${depth}\0${budget}\0${path}`, async () => {
                try {
                    const result = await inner.readdir(ctx, path, readOpts);
                    ingest(path, depth, result);
                    return result;
                }
                catch (err) {
                    const code = missingCode(err);
                    if (code)
                        putMissing(path, code);
                    throw err;
                }
            });
        }
        catch (err) {
            const code = missingCode(err);
            throw code ? fsError(code, ctx) : err;
        }
    }
    /**
     * Answer a walk's next directory by listing the whole subtree its parent sits on.
     *
     * A tree walk asks for one directory, descends into each of its subdirectories, and asks
     * again — so over a network mount it pays a round trip per directory, and a `node_modules`
     * with two thousand of them costs two thousand requests. That is what took a ripgrep over
     * one workspace to ~1000 `GET /fs/readdir`, the last 44 of them answered with 429.
     *
     * Nothing about the *first* listing says a walk is happening, and a single `ls` must not
     * drag a subtree over the wire. The signal is the second one: a miss inside a directory
     * whose own listing this cache has already answered is a descent, and a descent is a walk.
     * So the seed is rooted at the **parent** rather than at the path that missed, which is
     * what makes it cover the siblings the walk is about to ask for too — one request for a
     * directory with fifty subdirectories in it instead of fifty-one.
     *
     * This is the general form of what `../../worker/module/resolve.ts` does for node_modules.
     * That one can seed on sight because it knows the shape of what it is looking at; here the
     * only thing to go on is the descent, and every walk gets it rather than only the
     * resolver's.
     *
     * Returns whether the seed landed, which is a statement about the request and not about
     * `path`: the reply may show it complete, incomplete, or gone.
     */
    async function seedSubtree(ctx, path) {
        if (!prefetch)
            return false;
        const parentPath = dirname(path);
        if (parentPath === path)
            return false;
        // Never the provider's own root. puterfs refuses a recursive listing there — it
        // would be a prefix scan over every user, see ./puter-readdir.ts — so the one
        // request this could make is a request that cannot succeed. The walk seeds one level
        // down instead, which costs it a single extra listing.
        if (parentPath === "/")
            return false;
        const parent = dirNodeAt(parentPath);
        // Not a descent. Nobody has listed the parent, so there is no reason to believe
        // anything else in this subtree is about to be asked for.
        if (!parent || parent.askedGen !== gen)
            return false;
        // Once per root, recorded *before* the request rather than after: an overrun or a
        // failure retried once per sibling is the one shape that makes this cost more than
        // no seeding at all.
        if (parent.seededGen === gen)
            return false;
        // Nothing to gain from filling a tree that is not currently allowed to answer.
        if (!(await believable()))
            return false;
        parent.seededGen = gen;
        bump("readdir.seed");
        try {
            await listThrough(ctx, parentPath, prefetch.depth, {
                recursive: true,
                depth: prefetch.depth,
                maxEntries: prefetch.maxEntries,
            });
            return true;
        }
        catch {
            // Best effort by construction: the caller's own listing is the next line and will
            // report whatever this ran into, against their context rather than this one.
            bump("readdir.seed.failed");
            return false;
        }
    }
    async function readThrough(ctx, path) {
        try {
            return await once(`readFile\0${path}`, async () => {
                try {
                    const bytes = await inner.readFile(ctx, path);
                    if (maxBytes > 0 && bytes.length <= maxFileBytes) {
                        // The backend allocated these, so they are ours to hold. A
                        // *write's* buffer is not — see `writeFile`.
                        const node = { kind: "file", gen };
                        const previous = walk(path).node;
                        // Keep a stat only if it is still believed; a stale one would
                        // pair fresh bytes with an identity nothing has verified.
                        if (previous?.kind === "file" && live(previous)) {
                            node.entry = previous.entry;
                        }
                        replace(path, node);
                        admit(node, bytes);
                    }
                    return bytes;
                }
                catch (err) {
                    const code = missingCode(err);
                    if (code)
                        putMissing(path, code);
                    throw err;
                }
            });
        }
        catch (err) {
            const code = missingCode(err);
            throw code ? fsError(code, ctx) : err;
        }
    }
    /**
     * Every entry at or below `dir`, down to `depth` levels.
     *
     * Undefined when the tree holds a child it cannot describe — a directory node
     * created to hold something below it, with no stat of its own. Answering
     * without it would be a `complete: true` listing that silently omits a real
     * entry, which is the one way a cached listing can be actively wrong.
     */
    function collect(dir, depth) {
        const out = [];
        const visit = (node, left) => {
            if (left <= 0)
                return true;
            for (const child of node.children.values()) {
                if (child.kind === "missing")
                    continue;
                if (!child.entry)
                    return false;
                out.push(child.entry);
                if (child.kind === "dir" && !visit(child, left - 1))
                    return false;
            }
            return true;
        };
        return visit(dir, depth) ? out : undefined;
    }
    /**
     * A `readdir` answered from the tree, or undefined when the tree cannot answer it.
     *
     * Throws for a path that is not a directory — which is an answer, and one this can give
     * without a request.
     */
    function listFromTree(ctx, node, want, budget) {
        if (node?.kind === "missing")
            throw fsError(node.code, ctx);
        if (node?.kind === "file")
            throw fsError("ENOTDIR", ctx);
        if (node?.kind !== "dir" || node.depth < want)
            return undefined;
        const entries = collect(node, want);
        if (!entries)
            return undefined;
        // Answering past the caller's budget would answer a different question: `complete` is
        // what tells them whether a negative may be derived from this, and a truncated listing
        // carries no such licence.
        return entries.length > budget
            ? { entries: entries.slice(0, budget), complete: false }
            : { entries, complete: true };
    }
    /**
     * Record that a directory's own listing was answered, and how deep a question it answered.
     *
     * `seedSubtree` reads the first of these to recognise a descent. A *recursive* answer also
     * counts as having seeded that root, because it is the same request seeding would have
     * made — without which the walk's first miss below it would immediately ask for it again.
     */
    function markAnswered(path, want) {
        const dir = dirNodeAt(path);
        if (!dir)
            return;
        dir.askedGen = gen;
        if (want > 1)
            dir.seededGen = gen;
    }
    // ------------------------------------------------------------------- the ops
    const provider = {
        name: `cache(${inner.name})`,
        async stat(ctx, path) {
            const node = await believe(ctx, path);
            if (node?.kind === "missing") {
                bump("stat.hit");
                throw fsError(node.code, ctx);
            }
            if (node?.entry) {
                bump("stat.hit");
                return node.entry;
            }
            bump("stat.miss");
            return statThrough(ctx, path);
        },
        async readdir(ctx, path, readOpts) {
            // `recursive` with no depth means "everything", which the backend serves
            // with a horizon walk — so what it establishes is unbounded, not MAX_DEPTH.
            const want = readOpts?.recursive ? (readOpts.depth ?? UNBOUNDED) : 1;
            const budget = readOpts?.maxEntries ?? Infinity;
            const cached = listFromTree(ctx, await believe(ctx, path, false), want, budget);
            if (cached) {
                bump("readdir.hit");
                markAnswered(path, want);
                return cached;
            }
            bump("readdir.miss");
            // A walk pays a request per directory it descends into; seeding the parent's
            // subtree on the first descent makes it pay one per subtree. Only for a plain
            // listing — a caller who asked recursively is already asking for a subtree.
            if (want === 1 && (await seedSubtree(ctx, path))) {
                const seeded = listFromTree(ctx, await believe(ctx, path, false), want, budget);
                if (seeded) {
                    bump("readdir.seeded");
                    markAnswered(path, want);
                    return seeded;
                }
            }
            const listing = await listThrough(ctx, path, want, readOpts);
            markAnswered(path, want);
            return listing;
        },
        async readFile(ctx, path) {
            const node = await believe(ctx, path);
            if (node?.kind === "missing") {
                bump("readFile.hit");
                throw fsError(node.code, ctx);
            }
            if (node?.kind === "dir") {
                bump("readFile.hit");
                throw fsError("EISDIR", ctx);
            }
            if (node?.kind === "file" && node.bytes) {
                bump("readFile.hit");
                touch(node);
                return node.bytes;
            }
            bump("readFile.miss");
            return readThrough(ctx, path);
        },
        async writeFile(ctx, path, data) {
            await inner.writeFile(ctx, path, data);
            // The bytes are now known exactly; the stat is not. Only the backend can
            // say what size and mtime it recorded — puterfs stamps its own clock at
            // one-second resolution — and inventing them to keep the enclosing
            // listing intact would put a wrong `Stats` in front of every caller that
            // lists this directory. So the entry goes, and with it the ancestors'
            // claim to know their own contents.
            //
            // Keeping the bytes is what this is really for: an agent that writes a
            // file and reads it straight back pays nothing, which is the common half
            // of an edit.
            const node = { kind: "file", gen };
            invalidate(path, node);
            // A copy: what arrives is a view into the request frame, whose buffer the
            // transport may reuse the moment this returns.
            if (maxBytes > 0 && data.length <= maxFileBytes) {
                admit(node, new Uint8Array(data));
            }
            statfsValue = undefined;
        },
        async mkdir(ctx, path, mkdirOpts) {
            const created = await inner.mkdir(ctx, path, mkdirOpts);
            // A recursive mkdir may have created ancestors too, and reports at most
            // one of them, so nothing narrower than the whole chain is safe.
            invalidate(path);
            statfsValue = undefined;
            return created;
        },
        async rm(ctx, path, rmOpts) {
            await inner.rm(ctx, path, rmOpts);
            invalidate(path, { kind: "missing", code: "ENOENT", gen });
            statfsValue = undefined;
        },
        async rename(ctx, from, to) {
            await inner.rename(ctx, from, to);
            invalidate(from, { kind: "missing", code: "ENOENT", gen });
            invalidate(to);
        },
        async utimes(ctx, path, atimeMs, mtimeMs) {
            const applied = await inner.utimes(ctx, path, atimeMs, mtimeMs);
            if (!applied)
                return applied;
            // The timestamps moved; the contents did not. Dropping the stat while
            // keeping the bytes is the honest description of what changed — though a
            // later revalidation cannot confirm bytes it has no identity for, so they
            // go on the next stale mark.
            const node = walk(path).node;
            if (node && node.kind !== "missing") {
                if (node.entry?.uid)
                    byUid.delete(node.entry.uid);
                node.entry = undefined;
            }
            return applied;
        },
    };
    // ------------------------------------------- optional halves of the interface
    //
    // Mirrored from the wrapped provider rather than declared unconditionally,
    // because the mount snapshot derives its capability flags from which of these
    // exist and the worker picks a read strategy from those. Declaring one the
    // backend does not have would make the advertised capability a lie — and
    // over-claiming `hasNativeRange` in particular is quadratic.
    if (inner.readRange) {
        provider.readRange = async (ctx, path, offset, length) => {
            const node = await believe(ctx, path);
            if (node?.kind === "file" && node.bytes) {
                bump("readRange.hit");
                touch(node);
                return node.bytes.subarray(offset, offset + length);
            }
            bump("readRange.miss");
            // Deliberately not stored: a window of a file is not the file, and
            // admitting it as one is how a positioned read silently becomes a short
            // one.
            return inner.readRange(ctx, path, offset, length);
        };
    }
    if (inner.openRead) {
        provider.openRead = async (ctx, path, range) => {
            const node = await believe(ctx, path);
            if (node?.kind === "file" && node.bytes) {
                bump("openRead.hit");
                touch(node);
                return streamOfBytes(node.bytes, range);
            }
            bump("openRead.miss");
            // Passed through without being held: a stream is what a caller reaches for
            // when a file is too big to want in memory, which is exactly the file this
            // should not be holding.
            return inner.openRead(ctx, path, range);
        };
    }
    if (inner.copyFile) {
        provider.copyFile = async (ctx, from, to, copyOpts) => {
            await inner.copyFile(ctx, from, to, copyOpts);
            invalidate(to);
            statfsValue = undefined;
        };
    }
    if (inner.statfs) {
        provider.statfs = async (ctx) => {
            if (statfsValue && Date.now() - statfsAt < STATFS_TTL_MS) {
                bump("statfs.hit");
                return statfsValue;
            }
            bump("statfs.miss");
            statfsValue = await inner.statfs(ctx);
            statfsAt = Date.now();
            return statfsValue;
        };
    }
    // ------------------------------------------------------------------- control
    provider.applyEvent = (event) => {
        if (!under(prefix, event.path))
            return;
        const path = toLocal(prefix, event.path);
        bump("event");
        // A uid we hold at some *other* path means that entry moved and nothing said
        // so — the api answers a rename with `item.updated` naming only the new path.
        // Without this the old name stays cached, and believed, forever.
        if (event.uid) {
            const previous = byUid.get(event.uid);
            if (previous !== undefined && previous !== path) {
                invalidate(previous, { kind: "missing", code: "ENOENT", gen });
            }
        }
        if (event.kind === "removed") {
            if (event.descendantsOnly)
                invalidateChildren(path);
            else
                invalidate(path, { kind: "missing", code: "ENOENT", gen });
            return;
        }
        if (event.kind === "moved" &&
            event.oldPath &&
            under(prefix, event.oldPath)) {
            invalidate(toLocal(prefix, event.oldPath), {
                kind: "missing",
                code: "ENOENT",
                gen,
            });
        }
        invalidate(path);
    };
    provider.markStale = () => {
        bump("stale");
        gen++;
        statfsValue = undefined;
    };
    provider.flush = () => {
        bump("flush");
        gen++;
        flushTree();
    };
    provider.stats = () => ({
        ...Object.fromEntries([...counts].sort((a, b) => b[1] - a[1])),
        bytesHeld: held,
        filesHeld: lru.size,
        generation: gen,
    });
    return provider;
}

// Parsing an `fs.open` flags string.
//
// Shared because both sides need it now: the worker validates what a program passed, and the
// host actually opens the file and has to know whether to create, truncate or append. One copy,
// so `"a+"` cannot mean two different things depending on which side is asking.
/**
 * The O_* bits that change what an open *does*, at their Linux values.
 *
 * Node exposes the host platform's numbers as `fs.constants`, so in principle these belong to
 * a platform. In practice the only numbers that reach this runtime are Linux's: that is what
 * `fs.constants` reports in a browser build, and what Go's js/wasm `syscall` package hardcodes.
 */
const O_ACCMODE = 0o3;
const O_RDONLY = 0o0;
const O_WRONLY = 0o1;
const O_RDWR = 0o2;
const O_CREAT = 0o100;
const O_EXCL = 0o200;
const O_TRUNC = 0o1000;
const O_APPEND = 0o2000;
/**
 * A numeric `flags` bitmask, which node accepts everywhere it accepts a string.
 *
 * Every other bit is ignored rather than rejected — `O_CLOEXEC`, `O_NOCTTY`, `O_NONBLOCK`,
 * `O_SYNC` and friends describe how a real descriptor behaves, and there is nothing here for
 * them to mean. Ignoring them is much closer to node than refusing the open.
 */
function parseNumericFlags(flags) {
    const access = flags & O_ACCMODE;
    // 3 is the one access mode with no meaning; linux rejects it and so does this.
    if (access !== O_RDONLY && access !== O_WRONLY && access !== O_RDWR) {
        throw fsError("EINVAL", { syscall: "open", message: "invalid flags" });
    }
    return {
        flag: flags,
        read: access === O_RDONLY || access === O_RDWR,
        write: access === O_WRONLY || access === O_RDWR,
        append: (flags & O_APPEND) !== 0,
        create: (flags & O_CREAT) !== 0,
        truncateOnOpen: (flags & O_TRUNC) !== 0,
        exclusive: (flags & O_EXCL) !== 0,
    };
}
// Parses an fs open() flags argument ("r", "w+", "ax", ..., or an O_* bitmask) into the
// booleans the handle implementations care about.
function parseOpenFlags(flags) {
    if (flags === undefined)
        flags = "r";
    if (typeof flags === "number")
        return parseNumericFlags(flags);
    const aliases = {
        rs: "r",
        "rs+": "r+",
        as: "a",
        "as+": "a+",
    };
    const normalized = aliases[flags] ?? flags;
    const table = {
        r: {
            flag: "r",
            read: true,
            write: false,
            append: false,
            create: false,
            truncateOnOpen: false,
            exclusive: false,
        },
        "r+": {
            flag: "r+",
            read: true,
            write: true,
            append: false,
            create: false,
            truncateOnOpen: false,
            exclusive: false,
        },
        w: {
            flag: "w",
            read: false,
            write: true,
            append: false,
            create: true,
            truncateOnOpen: true,
            exclusive: false,
        },
        "w+": {
            flag: "w+",
            read: true,
            write: true,
            append: false,
            create: true,
            truncateOnOpen: true,
            exclusive: false,
        },
        wx: {
            flag: "wx",
            read: false,
            write: true,
            append: false,
            create: true,
            truncateOnOpen: true,
            exclusive: true,
        },
        "wx+": {
            flag: "wx+",
            read: true,
            write: true,
            append: false,
            create: true,
            truncateOnOpen: true,
            exclusive: true,
        },
        a: {
            flag: "a",
            read: false,
            write: true,
            append: true,
            create: true,
            truncateOnOpen: false,
            exclusive: false,
        },
        "a+": {
            flag: "a+",
            read: true,
            write: true,
            append: true,
            create: true,
            truncateOnOpen: false,
            exclusive: false,
        },
        ax: {
            flag: "ax",
            read: false,
            write: true,
            append: true,
            create: true,
            truncateOnOpen: false,
            exclusive: true,
        },
        "ax+": {
            flag: "ax+",
            read: true,
            write: true,
            append: true,
            create: true,
            truncateOnOpen: false,
            exclusive: true,
        },
    };
    const parsed = table[normalized];
    if (!parsed)
        throw fsError("EINVAL", { syscall: "open", message: "invalid flags" });
    return parsed;
}

// Operations that are compositions of other operations rather than backend primitives.
//
// These sit above the mount layer on purpose: `cp` between two different mounts has to
// become read-then-write, and `truncate` on any backend is read-modify-write because no
// backend here can write part of a file. Pushing them down into providers would mean every
// provider reimplementing them.
//
// They live *here*, on the host, rather than in the worker, and that is the whole reason
// every `fs.*Sync` call is now exactly one blocking round trip. As worker-side compositions
// each of these cost one round trip per step: `appendFileSync` was two, `truncateSync` two,
// `cpSync` of a tree O(entries), `mkdtempSync` one plus its mkdir. Composed next to the
// providers they are one each.
//
// The exception is `cp` with a filter, which cannot come here at all: the filter is a user
// callback living in the worker, so a filtered copy stays a worker-driven walk over the
// primitives. That is the same reason `fs.promises.cp` has always had its own
// implementation — a user callback in the middle of an operation, not anything about the
// transport.
/** Generates a 6-character random suffix for mkdtemp(), matching node's length. */
function randomTempSuffix() {
    const alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let out = "";
    for (let i = 0; i < 6; i++) {
        out += alphabet[Math.floor(Math.random() * alphabet.length)];
    }
    return out;
}
function isMissing(err) {
    const code = err.code;
    return code === "ENOENT" || code === "ENOTDIR";
}
/**
 * Existence probe. puterfs has no real permission bits — `Stats.mode` is a constant
 * `type | 0o777` — so R/W/X_OK can never fail and only F_OK is a real question, which is
 * what the stat answers.
 */
async function exists(fs, ctx, path) {
    try {
        await fs.stat(ctx, path);
        return true;
    }
    catch (err) {
        if (isMissing(err))
            return false;
        throw err;
    }
}
async function append(fs, ctx, path, data) {
    // Reading the existing bytes rather than text: appending is a byte operation, and
    // decoding then re-encoding would corrupt any file that isn't valid text in the
    // requested encoding.
    let old;
    try {
        old = await fs.readFile(ctx, path);
    }
    catch (err) {
        // ONLY a missing file means "start from empty". A bare `catch` here would turn
        // every other failure — a 500, EACCES, EISDIR, an aborted read — into a silent
        // truncation: the append would proceed with an empty base and write back only the
        // new data, destroying the file it was supposed to extend.
        if (!isMissing(err))
            throw err;
        old = new Uint8Array(0);
    }
    const out = new Uint8Array(old.length + data.length);
    out.set(old, 0);
    out.set(data, old.length);
    await fs.writeFile(ctx, path, out);
}
async function truncate(fs, ctx, path, len) {
    if (!Number.isInteger(len))
        len = Math.trunc(len);
    if (len < 0)
        len = 0;
    // Truncating to zero needs no read at all — the old contents are being discarded. The
    // general path below would download the whole file first just to drop it.
    if (len === 0) {
        await fs.writeFile(ctx, path, new Uint8Array(0));
        return;
    }
    const buf = await fs.readFile(ctx, path);
    let out;
    if (len <= buf.length) {
        out = buf.subarray(0, len);
    }
    else {
        // Growing a file zero-fills, as ftruncate(2) does.
        out = new Uint8Array(len);
        out.set(buf, 0);
    }
    await fs.writeFile(ctx, path, out);
}
async function mkdtemp(fs, ctx, prefix) {
    const path = prefix + randomTempSuffix();
    await fs.mkdir({ ...ctx, syscall: "mkdir" }, path, { recursive: false });
    return path;
}
async function rmrf(fs, ctx, path, force) {
    await fs.rm(ctx, path, { recursive: true, force });
}
/**
 * Recursive copy, without a filter — see the note at the top of this file for why a
 * filtered copy cannot be composed here.
 */
async function cp(fs, ctx, source, destination, opts) {
    const force = opts.force !== false;
    const errorOnExist = opts.errorOnExist || false;
    const recursive = opts.recursive || false;
    async function copyEntry(src, dest) {
        const srcStat = await fs.stat({ syscall: "stat", reportPath: src }, src);
        if (srcStat.isDir) {
            if (!recursive) {
                throw fsError("EISDIR", {
                    syscall: "cp",
                    path: src,
                    message: "recursive option not enabled, cannot copy a directory",
                });
            }
            await fs.mkdir({ syscall: "mkdir", reportPath: dest }, dest, {
                recursive: true,
            });
            const listing = await fs.readdir({ syscall: "scandir", reportPath: src }, src);
            for (const entry of listing.entries) {
                await copyEntry(join(src, entry.name), join(dest, entry.name));
            }
            return;
        }
        if (await exists(fs, { syscall: "stat", reportPath: dest }, dest)) {
            if (errorOnExist)
                throw fsError("EEXIST", { syscall: "cp", path: dest });
            if (!force)
                return;
        }
        await fs.copyFile({ syscall: "copyfile", reportPath: src }, src, dest, {
            overwrite: true,
        });
    }
    await copyEntry(source, destination);
}
/**
 * `utimes`, with the validation a backend that cannot represent the request still owes.
 *
 * A provider returning `false` means "I could not apply this", which is a normal answer —
 * puterfs can only set a timestamp to *now*, and a `FileSystemFileHandle` cannot set one at
 * all. But node still reports ENOENT for a missing path, so when nothing was applied the
 * path has to be validated some other way, and a stat is that way.
 */
async function utimes(fs, ctx, path, atimeMs, mtimeMs) {
    const applied = await fs.utimes(ctx, path, atimeMs, mtimeMs);
    if (!applied)
        await fs.stat({ syscall: "stat", reportPath: path }, path);
    return applied;
}

// Frame in, frame out. The one entry point both transports call.
//
// The synchronous path (a blocking XHR relayed by the service worker) and the asynchronous
// path (a postMessage) both land here, on the same bytes, through the same dispatch table.
// That is deliberate: the failure mode this boundary is most exposed to is a bug that
// exists on one transport and not the other, and there is nothing to diverge if there is
// only one implementation.
//
// It is also the seam that keeps the host VFS movable. Nothing here knows whether the caller
// is a service worker, a window, or a dedicated worker the page owns — so relaying frames to
// a VFS host worker later (which is where OPFS is fastest, since `createSyncAccessHandle` is
// worker-only) is a change to the plumbing above, not to this file.
/**
 * Ops whose answer depends on a file's current contents, size or mtime, addressed by path.
 *
 * `open` is in the set because it stats the file to seed the new handle's size, and a second fd
 * onto a file with a dirty first fd would otherwise start from the stale length. `readdir` is not:
 * its path is a directory, and no handle is ever open on one.
 */
const READS_THROUGH_PATH = new Set([
    "stat",
    "access",
    "exists",
    "readFile",
    "readRange",
    "copyFile",
    "rename",
    "truncate",
    "cp",
    "open",
]);
async function handleFrame(deps, frame) {
    let request;
    let parts;
    try {
        const decoded = decodeFrame(frame);
        request = decoded.header;
        // Sideband bytes are not this call's. `fdWritev` writes every part it is given, so
        // letting a piggybacked stdout chunk through here would write it into the file.
        parts = primaryParts(decoded.header, decoded.parts);
    }
    catch (err) {
        // A frame we cannot even parse has no seq to echo. Answer with one anyway so the
        // worker gets a node-shaped error rather than a decode failure of its own.
        return {
            frame: reply({
                seq: 0,
                result: { ok: false, error: toWireError(err, "read") },
            }),
        };
    }
    const seq = request.seq;
    // A repeat means the worker retried after a transport failure. Answer from the record rather
    // than running the operation again — the whole point of the retry being safe.
    const already = recall(deps.replies, deps.sid, seq);
    if (already)
        return { frame: already };
    let answer;
    try {
        answer = await perform(deps, request.call, parts);
    }
    catch (err) {
        const failed = reply({
            seq,
            result: {
                ok: false,
                error: toWireError(err, ctxOf(request.call)?.syscall),
            },
        }, deps);
        remember(deps.replies, deps.sid, seq, failed);
        return { frame: failed };
    }
    const ok = reply({ seq, result: { ok: true, value: answer.value } }, deps, answer.parts);
    // A reply carrying a handle is not replayable: the handle is transferred, so the record
    // would hand a second caller a stream that has already been detached. The retry it exists
    // for cannot happen anyway — the op is async-only, and only the sync path retries.
    if (!answer.transfer)
        remember(deps.replies, deps.sid, seq, ok);
    return { frame: ok, transfer: answer.transfer };
}
function ctxOf(call) {
    return "ctx" in call ? call.ctx : undefined;
}
// The sidebands are drained here rather than in ../../wire/router.ts because only this
// side knows what is pending: the router builds the message, the filesystem decides what
// rides along on it.
function reply(body, deps, parts) {
    return encodeReply(KIND_FS, body, parts, deps && {
        events: deps.drainEvents(),
        invalidate: deps.drainInvalidations(),
        apiCalls: deps.drainApiCalls?.(),
    });
}
/** No bytes, just a value. */
const v = (value) => ({ value });
/** Bytes in the payload; the header's value is null. */
const b = (bytes) => ({ value: null, parts: [bytes] });
async function perform(deps, call, parts) {
    const { fs } = deps;
    // An open fd buffers its writes host-side (see HandleRegistry#flushPath). Anything that then
    // reads the same file *by path* has to see them, so those ops flush first. Listed explicitly
    // rather than inferred from "has a path" because the write-side ops must NOT appear here: a
    // path-level `writeFile` racing a dirty fd is last-writer-wins either way, and flushing first
    // would just make the fd's stale buffer the winner.
    if (READS_THROUGH_PATH.has(call.op)) {
        const target = call.path ??
            call.from;
        if (typeof target === "string")
            await deps.handles.flushPath(target);
    }
    switch (call.op) {
        // Answered without touching the filesystem: this is the startup probe that verifies
        // the service worker is really intercepting, and it echoes the session id back so a
        // misrouted request is caught rather than silently served.
        case "probe":
            return v({ proto: deps.proto, sid: deps.sid });
        case "mounts":
            return v(deps.table.snapshot());
        case "stat":
            return v(await fs.stat(call.ctx, call.path));
        case "access":
            return v((await fs.stat(call.ctx, call.path), null));
        case "exists":
            return v(await exists(fs, call.ctx, call.path));
        case "statfs":
            return v(await fs.statfs(call.ctx, call.path));
        case "readdir":
            return v(await fs.readdir(call.ctx, call.path, call.opts));
        case "readFile":
            return b(await fs.readFile(call.ctx, call.path));
        case "readRange":
            return b(await fs.readRange(call.ctx, call.path, call.offset, call.length));
        case "writeFile":
            await fs.writeFile(call.ctx, call.path, payload(parts));
            return v(null);
        case "append":
            await append(fs, call.ctx, call.path, payload(parts));
            return v(null);
        case "mkdir":
            return v(await fs.mkdir(call.ctx, call.path, { recursive: call.recursive }));
        case "rm":
            await fs.rm(call.ctx, call.path, {
                recursive: call.recursive,
                force: call.force,
            });
            return v(null);
        case "rmrf":
            await rmrf(fs, call.ctx, call.path, call.force);
            return v(null);
        case "rename":
            await fs.rename(call.ctx, call.from, call.to);
            return v(null);
        case "copyFile":
            await fs.copyFile(call.ctx, call.from, call.to, {
                overwrite: call.overwrite,
            });
            return v(null);
        case "utimes":
            return v(await utimes(fs, call.ctx, call.path, call.atimeMs, call.mtimeMs));
        case "truncate":
            await truncate(fs, call.ctx, call.path, call.length);
            return v(null);
        case "mkdtemp":
            return v(await mkdtemp(fs, call.ctx, call.prefix));
        case "cp":
            await cp(fs, call.ctx, call.from, call.to, {
                recursive: call.recursive,
                force: call.force,
                errorOnExist: call.errorOnExist,
            });
            return v(null);
        // ------------------------------------------------------------ the fd family
        case "open": {
            // The read strategy is fixed for the fd's lifetime from the mount's capability,
            // which is why this is resolved here rather than guessed by the handle.
            const mount = deps.table.resolve(call.path).mount;
            const { fd } = await deps.handles.open(call.path, parseOpenFlags(call.flags), !!mount.provider.readRange, 
            // Whose fd this is. One `NodeVfs` may back several workers, and `closeSession`
            // has to be able to drop this one's descriptors without touching theirs.
            deps.sid);
            return v({ fd });
        }
        case "close":
            await deps.handles.close(call.fd);
            return v(null);
        case "fdRead":
            return b(await deps.handles.get(call.fd, "read").read(call.length, call.position));
        case "fdReadv": {
            const chunks = await deps.handles
                .get(call.fd, "readv")
                .readv(call.lengths, call.position);
            return { value: null, parts: chunks };
        }
        case "fdWrite":
            return v(await deps.handles
                .get(call.fd, "write")
                .write(payload(parts), call.position));
        case "fdWritev":
            return v(await deps.handles.get(call.fd, "writev").writev(parts, call.position));
        case "fdReadFile":
            return b(await deps.handles.get(call.fd, "read").readFile());
        case "fdWriteFile":
            await deps.handles.get(call.fd, "write").writeFile(payload(parts));
            return v(null);
        case "fdAppend":
            await deps.handles.get(call.fd, "write").appendFile(payload(parts));
            return v(null);
        case "fdStat":
            return v(await deps.handles.get(call.fd, "fstat").stat());
        case "fdTruncate":
            await deps.handles.get(call.fd, "ftruncate").truncate(call.length);
            return v(null);
        case "fdSync":
            await deps.handles.get(call.fd, "fsync").sync();
            return v(null);
        case "fdUtimes":
            return v(await deps.handles
                .get(call.fd, "futimes")
                .utimes(call.atimeMs, call.mtimeMs));
        case "fs.openRead": {
            // The stream is the answer, and it is transferred rather than encoded — see the
            // note on `Answer.transfer`. `size` is whatever the host already knew, so a
            // consumer that wants a length does not have to stat separately.
            const opened = call.fd !== undefined
                ? await deps.openReadFd(call.fd, {
                    start: call.start ?? 0,
                    end: call.end,
                })
                : await deps.openRead(call.path, {
                    start: call.start ?? 0,
                    end: call.end,
                });
            return {
                value: { size: opened.size },
                transfer: [opened.stream],
            };
        }
        case "fdFlushWrite": {
            // write + sync + close as one op, which is what `Utf8Stream.flushSync` wants and
            // what used to cost it five blocking round trips.
            const handle = deps.handles.get(call.fd, "write");
            if (parts.length)
                await handle.write(payload(parts), null);
            await handle.sync();
            await deps.handles.close(call.fd);
            return v(null);
        }
    }
    // Exhaustive over VfsCall; a new op that forgets a branch fails to compile.
    const never = call;
    throw new Error(`unknown vfs op: ${JSON.stringify(never)}`);
}
function payload(parts) {
    return parts[0] ?? new Uint8Array(0);
}

// `/dev`, enough of it to be useful: `null`, `zero`, `full`.
//
// `>/dev/null` is not an optional nicety — it is the single most common redirect in shell code, and
// a shell running against a filesystem has to *write to the file* to honour it. Without the file
// every such command fails, and it fails at the redirect rather than in the command, so the error
// names a path the author never typed. That is exactly what happened here: Claude Code 2.1.220
// prefixes every command it builds with `{ … } >/dev/null 2>&1 || true`, so `true` and `echo hello`
// failed identically with "no such file or directory: /dev/null".
//
// `os.devNull` already reported "/dev/null" (src/worker/node/os.ts), so the runtime was naming a
// path it did not provide.
//
// Writes are **discarded**, not stored. A memory-backed file would have worked for the redirect and
// then grown without bound the first time something piped real volume into it — which is the kind of
// slow, invisible wrongness a device node exists to avoid.
/** How much `/dev/zero` will hand over in one read. Unbounded is not an option in a browser. */
const ZERO_READ_LIMIT = 1024 * 1024;
const DEVICES = new Set(["null", "zero", "full"]);
function localName(path) {
    // The facade hands the provider a mount-local path: "/" for /dev itself, "/null" below it.
    return path.replace(/^\/+/, "").replace(/\/+$/, "");
}
function entry(path, name, isDir) {
    return {
        path,
        name,
        uid: "",
        isDir,
        isSymlink: false,
        size: 0,
        modifiedMs: 0,
        createdMs: 0,
        accessedMs: 0,
    };
}
function createDevProvider(opts = {}) {
    opts.events ?? NO_EVENTS;
    const device = (path, ctx) => {
        const name = localName(path);
        if (!DEVICES.has(name))
            throw fsError("ENOENT", ctx);
        return name;
    };
    return {
        name: "dev",
        async stat(ctx, path) {
            const name = localName(path);
            if (name === "")
                return entry(path, "dev", true);
            if (!DEVICES.has(name))
                throw fsError("ENOENT", ctx);
            return entry(path, name, false);
        },
        async readdir(ctx, path) {
            if (localName(path) !== "")
                throw fsError("ENOTDIR", ctx);
            return {
                entries: [...DEVICES].map((name) => entry(`/${name}`, name, false)),
                complete: true,
            };
        },
        async readFile(ctx, path) {
            const name = device(path, ctx);
            // `null` is at EOF immediately; `zero` and `full` read as zeroes. A real `/dev/zero` never
            // ends, which a whole-file read cannot express, so it is capped — a caller wanting a
            // stream of zeroes is better served by asking for a length.
            return name === "null"
                ? new Uint8Array(0)
                : new Uint8Array(ZERO_READ_LIMIT);
        },
        async writeFile(ctx, path, data) {
            const name = device(path, ctx);
            // `/dev/full` exists to fail, and is the only honest way to test an ENOSPC path.
            if (name === "full" && data.byteLength > 0)
                throw fsError("ENOSPC", ctx);
            // null and zero swallow everything, and store nothing.
        },
        async mkdir(ctx, path, opts) {
            // `mkdir -p /dev` has to succeed, because /dev already exists — that is what `recursive`
            // means, and refusing it is not a theoretical nicety: any writer that ensures the parent
            // directory before writing (which is most of them) would otherwise fail on
            // `>/dev/null` with the mkdir's error, naming a permission problem for a write that is
            // perfectly allowed.
            if (localName(path) === "") {
                if (opts.recursive)
                    return undefined;
                throw fsError("EEXIST", ctx);
            }
            // A new device node, on the other hand, is not something to invent.
            throw fsError("EPERM", ctx);
        },
        async rm(ctx) {
            throw fsError("EPERM", ctx);
        },
        async rename(ctx) {
            throw fsError("EPERM", ctx);
        },
        async utimes(ctx, path) {
            // The path has to exist for the ENOENT to be right, but a device has no timestamps to set.
            device(path, ctx);
            return false;
        },
    };
}

// Open files: one handle per fd, and the buffering that makes a partial write possible on a
// backend that has none.
//
// ## Why a handle needs a buffer at all
//
// puterfs has no partial-write primitive — every write is a whole-file upload — and a
// `FileSystemFileHandle` is not much better. So `fs.writeSync(fd, bytes, …, position)` has to
// become read-modify-write, and a handle holds the file's contents to make a sequence of small
// writes cost one upload at flush rather than one each. That buffer is also what lets `fstat`
// on a dirty fd report the size the program just wrote rather than the stale one on the server.
//
// ## What changed in moving here, and why it got simpler
//
// The worker's version could not take a lock. A "write" is a multi-round-trip
// read-modify-write over mutable state, so two overlapping async writes would interleave at
// every await and lose data — but a *synchronous* write cannot wait on a promise queue, and it
// could genuinely arrive while one was held (`let p = handle.readFile(); fs.readSync(fd, …)`).
// Rejecting that with EBUSY would have invented a failure mode node does not have. So the
// invariant moved into the code instead: offsets reserved in straight-line code before the
// first yield, buffer fills double-checked after every yield, and a generation counter to fail
// anything that resumed into a closed fd.
//
// None of that is needed here. Both surfaces arrive as wire ops through one dispatcher, and a
// host operation never needs the worker to make progress, so an ordinary FIFO queue per handle
// cannot deadlock a blocked caller. With operations serialized:
//
//   - offsets advance by the *actual* bytes moved, instead of being reserved optimistically and
//     rolled back on a short read;
//   - a fill cannot be raced by a write, so the double-checks go;
//   - nothing can resume into a changed generation, so the counter goes.
//
// Two concurrent positionless reads on one fd now serialize rather than interleaving. That is a
// stronger guarantee than node makes, not a weaker one.
/** node starts its own fds at 0; 10 leaves room for the stdio numbers the worker owns. */
const FIRST_FD = 10;
/**
 * A cap, so a worker that leaks handles is bounded.
 *
 * This matters more than it did worker-side: a handle now outlives the worker that opened it
 * unless the session is torn down, so an unbounded registry is a real leak of whatever those
 * handles hold — including, for a memory mount, the contents of unlinked files that are kept
 * alive on purpose.
 */
const MAX_OPEN = 4096;
function concat$1(a, b, at) {
    const size = Math.max(a.length, at + b.length);
    const out = new Uint8Array(size);
    out.set(a, 0);
    out.set(b, at);
    return out;
}
class Handle {
    fd;
    path;
    flags;
    /**
     * The sync session that opened this fd, so `closeAll` can drop one worker's handles
     * without touching another's. Undefined only for a handle opened outside a session,
     * which nothing does today — `dispatch.ts`'s `open` is the sole caller — and which
     * `closeAll(owner)` deliberately leaves alone rather than guessing about.
     */
    owner;
    #fs;
    #closed = false;
    #closing = false;
    #position = 0;
    /** Where the next append lands: the later of the file's length and any append already made. */
    #appendCursor = 0;
    /** Byte-range cache, used only when the backend has a real positioned read. */
    #fragments = [];
    /** Whole-file buffer: the truth once anything has been written. */
    #buffer;
    #bufferMtimeMs;
    /** Last known size on the backend, used to keep ranged reads inside the file. */
    #serverSize;
    #dirty = false;
    #hasNativeRange;
    /** FIFO. See the note at the top of this file for why this is allowed to exist now. */
    #tail = Promise.resolve();
    constructor(fd, path, flags, fs, hasNativeRange, owner) {
        this.fd = fd;
        this.path = path;
        this.flags = flags;
        this.#fs = fs;
        this.#hasNativeRange = hasNativeRange;
        this.owner = owner;
    }
    /** @internal — set by the registry right after a truncating open. */
    seedEmptied() {
        // No re-stat. The size is zero by construction, and the mtime was only ever used to
        // decide whether to re-read before a write — pointless for a handle about to replace
        // the whole file. Leaving the mtime undefined disables that revalidation, which is what
        // makes skipping the round trip safe.
        this.#serverSize = 0;
        this.#appendCursor = 0;
        this.#buffer = new Uint8Array(0);
        this.#bufferMtimeMs = undefined;
    }
    /** @internal */
    seedExisting(size) {
        this.#serverSize = size;
        this.#appendCursor = size;
    }
    get closed() {
        return this.#closed || this.#closing;
    }
    // --------------------------------------------------------------- the queue
    #run(fn) {
        // `then(fn, fn)` rather than `then(fn)`: a failed operation must not wedge the queue
        // behind it.
        const result = this.#tail.then(fn, fn);
        this.#tail = result.catch(() => undefined);
        return result;
    }
    /**
     * Admission control, then queue.
     *
     * The open check happens **here, at submission**, not inside the queued body. An operation
     * that was accepted before `close()` was called has already been issued as far as the caller
     * is concerned, and node lets it complete; only operations submitted *after* the close see
     * EBADF. Checking inside the body instead would fail everything still queued behind a close,
     * silently dropping writes the program believed it had made.
     */
    #submit(syscall, fn) {
        if (this.#closed || this.#closing) {
            return Promise.reject(fsError("EBADF", { syscall, path: this.path }));
        }
        return this.#run(fn);
    }
    #ctx(syscall) {
        return { syscall, reportPath: this.path };
    }
    #assertCanRead() {
        if (!this.flags.read)
            throw fsError("EBADF", { syscall: "read", path: this.path });
    }
    #assertCanWrite() {
        if (!this.flags.write) {
            throw fsError("EBADF", { syscall: "write", path: this.path });
        }
    }
    // ------------------------------------------------------------ buffer filling
    /** Ensure the buffer holds the file's current contents. Never discards unflushed writes. */
    async #fill(syscall) {
        if (this.#dirty)
            return this.#buffer;
        const c = this.#ctx(syscall);
        if (this.#buffer !== undefined) {
            // No recorded mtime means revalidation is deliberately off — see `seedEmptied`.
            if (this.#bufferMtimeMs === undefined)
                return this.#buffer;
            // Someone else may have changed the file since. Note the known limit: puterfs
            // timestamps have one-second resolution, so a read-modify-write inside one second is
            // not detectable this way.
            const current = await this.#fs.stat(c, this.path);
            if (current.modifiedMs === this.#bufferMtimeMs)
                return this.#buffer;
            this.#bufferMtimeMs = current.modifiedMs;
            this.#serverSize = current.size;
        }
        const fetched = await this.#fs.readFile(c, this.path);
        this.#buffer = fetched;
        this.#fragments = [];
        this.#serverSize = fetched.length;
        this.#appendCursor = Math.max(this.#appendCursor, fetched.length);
        return fetched;
    }
    /**
     * Fill, and make sure an mtime is recorded, so a later clean fill can tell whether someone
     * else has changed the file.
     */
    async #fillForWrite(syscall) {
        const first = this.#buffer === undefined;
        const buf = await this.#fill(syscall);
        if (first && !this.#dirty && this.#bufferMtimeMs === undefined) {
            const entry = await this.#fs.stat(this.#ctx(syscall), this.path);
            this.#bufferMtimeMs = entry.modifiedMs;
        }
        return buf;
    }
    // ----------------------------------------------------------- fragment cache
    #missingRanges(start, end) {
        if (end <= start)
            return [];
        const ranges = [];
        let cursor = start;
        for (const fragment of this.#fragments) {
            if (fragment.end <= cursor)
                continue;
            if (fragment.start >= end)
                break;
            if (fragment.start > cursor)
                ranges.push([cursor, Math.min(fragment.start, end)]);
            cursor = Math.max(cursor, Math.min(fragment.end, end));
            if (cursor >= end)
                break;
        }
        if (cursor < end)
            ranges.push([cursor, end]);
        return ranges;
    }
    #mergeFragments() {
        if (this.#fragments.length < 2)
            return;
        this.#fragments.sort((a, b) => a.start - b.start);
        const merged = [this.#fragments[0]];
        for (let i = 1; i < this.#fragments.length; i++) {
            const prev = merged[merged.length - 1];
            const curr = this.#fragments[i];
            if (curr.start > prev.end) {
                merged.push(curr);
                continue;
            }
            const start = prev.start;
            const end = Math.max(prev.end, curr.end);
            const data = new Uint8Array(end - start);
            data.set(prev.data, prev.start - start);
            data.set(curr.data, curr.start - start);
            merged[merged.length - 1] = { start, end, data };
        }
        this.#fragments = merged;
    }
    #addFragment(start, data) {
        if (data.length === 0)
            return;
        this.#fragments.push({
            start,
            end: start + data.length,
            data: new Uint8Array(data),
        });
        this.#mergeFragments();
    }
    async #ensureRanges(start, end) {
        const c = this.#ctx("read");
        // Never request a range starting at or past EOF: puterfs answers those with a 500, not
        // a 416. Re-stat before concluding EOF, so a file another client has grown since we
        // opened it is still readable.
        if (this.#serverSize === undefined || start >= this.#serverSize) {
            const entry = await this.#fs.stat(c, this.path);
            this.#serverSize = entry.size;
        }
        if (start >= this.#serverSize)
            return;
        const limit = Math.min(end, this.#serverSize);
        for (const [from, to] of this.#missingRanges(start, limit)) {
            const size = to - from;
            const data = await this.#fs.readRange(c, this.path, from, size);
            if (data.length === 0)
                break;
            this.#addFragment(from, data);
            if (data.length < size)
                break;
        }
    }
    #readFromFragments(target, targetOffset, start, end) {
        let cursor = start;
        let written = 0;
        for (const fragment of this.#fragments) {
            if (fragment.end <= cursor)
                continue;
            if (fragment.start > cursor)
                break;
            if (fragment.end > cursor) {
                const takeUntil = Math.min(fragment.end, end);
                const take = takeUntil - cursor;
                if (take <= 0)
                    continue;
                target.set(fragment.data.subarray(cursor - fragment.start, cursor - fragment.start + take), targetOffset + written);
                cursor += take;
                written += take;
                if (cursor >= end)
                    break;
            }
        }
        return written;
    }
    // --------------------------------------------------------------------- read
    /** Up to `length` bytes. `position === null` reads from — and advances — the handle's offset. */
    read(length, position) {
        return this.#submit("read", async () => {
            this.#assertCanRead();
            if (!Number.isInteger(length) || length < 0) {
                throw fsError("EINVAL", {
                    syscall: "read",
                    path: this.path,
                    message: "invalid length",
                });
            }
            if (length === 0)
                return new Uint8Array(0);
            // Serialized, so the offset can advance by what was actually read rather than being
            // reserved up front and rolled back on a short read.
            const start = position === null ? this.#position : position;
            const end = start + length;
            const out = new Uint8Array(length);
            let bytesRead = 0;
            if (this.#buffer !== undefined) {
                if (start < this.#buffer.length) {
                    bytesRead = Math.min(length, this.#buffer.length - start);
                    out.set(this.#buffer.subarray(start, start + bytesRead), 0);
                }
            }
            else if (this.#hasNativeRange) {
                await this.#ensureRanges(start, end);
                bytesRead = this.#readFromFragments(out, 0, start, end);
            }
            else {
                // No real positioned read on this backend, so holding the whole file once beats
                // slicing it out of a fresh full download per positioned read.
                const buf = await this.#fill("read");
                if (start < buf.length) {
                    bytesRead = Math.min(length, buf.length - start);
                    out.set(buf.subarray(start, start + bytesRead), 0);
                }
            }
            if (position === null)
                this.#position = start + bytesRead;
            return out.subarray(0, bytesRead);
        });
    }
    /** `readv`: several lengths in one round trip, stopping at the first short read. */
    readv(lengths, position) {
        return this.#submit("readv", async () => {
            this.#assertCanRead();
            const out = [];
            let at = position;
            for (const length of lengths) {
                // Reuses `read`'s body through the queue would deadlock, so the offset walk is
                // done here and the primitive is called directly.
                const chunk = await this.#readUnqueued(length, at);
                out.push(chunk);
                if (at !== null)
                    at += chunk.length;
                if (chunk.length < length)
                    break;
            }
            return out;
        });
    }
    /** The body of `read`, without the queue — for callers already holding it. */
    async #readUnqueued(length, position) {
        if (length === 0)
            return new Uint8Array(0);
        const start = position === null ? this.#position : position;
        const end = start + length;
        const out = new Uint8Array(length);
        let bytesRead = 0;
        if (this.#buffer !== undefined) {
            if (start < this.#buffer.length) {
                bytesRead = Math.min(length, this.#buffer.length - start);
                out.set(this.#buffer.subarray(start, start + bytesRead), 0);
            }
        }
        else if (this.#hasNativeRange) {
            await this.#ensureRanges(start, end);
            bytesRead = this.#readFromFragments(out, 0, start, end);
        }
        else {
            const buf = await this.#fill("read");
            if (start < buf.length) {
                bytesRead = Math.min(length, buf.length - start);
                out.set(buf.subarray(start, start + bytesRead), 0);
            }
        }
        if (position === null)
            this.#position = start + bytesRead;
        return out.subarray(0, bytesRead);
    }
    /** From the handle's offset to EOF, as node's `filehandle.readFile` does. */
    readFile() {
        return this.#submit("read", async () => {
            this.#assertCanRead();
            const buf = await this.#fill("read");
            const from = this.#position;
            this.#position = buf.length;
            return new Uint8Array(buf.subarray(from));
        });
    }
    // -------------------------------------------------------------------- write
    #splice(position, data) {
        const buf = this.#buffer ?? new Uint8Array(0);
        this.#buffer =
            position + data.length <= buf.length
                ? (() => {
                    // In place, but on a copy: the buffer may still be the array a provider
                    // handed back, and mutating that would rewrite the file underneath us.
                    const owned = new Uint8Array(buf);
                    owned.set(data, position);
                    return owned;
                })()
                : concat$1(buf, data, position);
        this.#dirty = true;
    }
    write(data, position) {
        return this.#submit("write", async () => {
            this.#assertCanWrite();
            await this.#fillForWrite("write");
            // O_APPEND means "past everything this handle knows about", which is the later of the
            // file's length and any append already made through it.
            //
            // For an append-flagged handle the two are provably equal — `#fill` raises the cursor
            // to any longer file it refetches, and every write here sets the cursor to the new
            // buffer length — so the `max` is belt-and-braces on this path and a mutation to it is
            // not observable. It is load-bearing in `appendFile`, where a *positioned* write can
            // extend the buffer past EOF without moving the cursor; there it is tested.
            const at = this.flags.append
                ? Math.max(this.#buffer?.length ?? 0, this.#appendCursor)
                : position === null
                    ? this.#position
                    : position;
            this.#splice(at, data);
            if (this.flags.append)
                this.#appendCursor = at + data.length;
            if (position === null || this.flags.append)
                this.#position = at + data.length;
            return data.length;
        });
    }
    writev(chunks, position) {
        return this.#submit("writev", async () => {
            this.#assertCanWrite();
            await this.#fillForWrite("writev");
            let total = 0;
            let at = position;
            for (const chunk of chunks) {
                const target = this.flags.append
                    ? Math.max(this.#buffer?.length ?? 0, this.#appendCursor)
                    : at === null
                        ? this.#position
                        : at;
                this.#splice(target, chunk);
                if (this.flags.append)
                    this.#appendCursor = target + chunk.length;
                if (at === null || this.flags.append)
                    this.#position = target + chunk.length;
                else
                    at = target + chunk.length;
                total += chunk.length;
            }
            return total;
        });
    }
    writeFile(data) {
        return this.#submit("write", async () => {
            this.#assertCanWrite();
            // The whole file is being replaced, so nothing needs reading first — but the fill
            // still runs, so mtime bookkeeping is settled for a later clean read.
            await this.#fillForWrite("write");
            this.#buffer = new Uint8Array(data);
            this.#dirty = true;
            this.#position = this.#buffer.length;
            this.#appendCursor = this.#buffer.length;
            this.#fragments = [];
        });
    }
    appendFile(data) {
        return this.#submit("write", async () => {
            this.#assertCanWrite();
            const buf = await this.#fillForWrite("write");
            const at = Math.max(buf.length, this.#appendCursor);
            this.#appendCursor = at + data.length;
            this.#splice(at, data);
            this.#position = at + data.length;
        });
    }
    truncate(len = 0) {
        return this.#submit("ftruncate", async () => {
            this.#assertCanWrite();
            if (!Number.isInteger(len))
                len = Math.trunc(len);
            if (len < 0)
                len = 0;
            const buf = await this.#fillForWrite("ftruncate");
            let out;
            if (len <= buf.length)
                out = new Uint8Array(buf.subarray(0, len));
            else {
                // Growing a file zero-fills, as ftruncate(2) does.
                out = new Uint8Array(len);
                out.set(buf, 0);
            }
            this.#buffer = out;
            this.#dirty = true;
            this.#appendCursor = len;
            this.#fragments = [];
        });
    }
    // --------------------------------------------------------------- flush/close
    /** Whether this fd is holding writes the backing store has not seen yet. */
    get dirty() {
        return this.#dirty;
    }
    sync() {
        return this.#run(() => this.#syncUnqueued());
    }
    async #syncUnqueued() {
        if (!this.#dirty || !this.#buffer)
            return;
        const buf = this.#buffer;
        // Cleared before the upload so a failure can restore it, rather than a later write
        // having its flag wiped when this completes.
        this.#dirty = false;
        try {
            await this.#fs.writeFile(this.#ctx("write"), this.path, buf);
        }
        catch (err) {
            this.#dirty = true;
            throw err;
        }
        this.#serverSize = buf.length;
        this.#fragments = [];
        // The file has changed and the provider has already announced it. Drop the recorded
        // mtime rather than spend a stat learning the new one; a later read serves this buffer.
        this.#bufferMtimeMs = undefined;
    }
    /**
     * Flush and retire the fd.
     *
     * `#closing` is set synchronously so nothing new is accepted, while whatever is already
     * queued still runs — with a FIFO there is no half-applied operation to strand, which is the
     * problem the generation counter used to solve.
     */
    close() {
        if (this.#closed)
            return Promise.resolve();
        this.#closing = true;
        return this.#run(async () => {
            try {
                await this.#syncUnqueued();
            }
            finally {
                this.#closed = true;
                this.#closing = false;
            }
        });
    }
    // ---------------------------------------------------------------------- stat
    stat() {
        return this.#submit("fstat", async () => {
            // A dirty handle's buffer IS the file as far as this fd is concerned, and node's
            // fstat on a dirty fd reports the real size. Synthesizing it is both more correct
            // than reporting the stale server size and one round trip cheaper.
            if (this.#dirty && this.#buffer) {
                const now = Date.now();
                return {
                    path: this.path,
                    name: basename(this.path),
                    uid: "",
                    isDir: false,
                    isSymlink: false,
                    size: this.#buffer.length,
                    modifiedMs: now,
                    createdMs: now,
                    accessedMs: now,
                };
            }
            return this.#fs.stat(this.#ctx("fstat"), this.path);
        });
    }
    utimes(atimeMs, mtimeMs) {
        return this.#submit("futimes", async () => {
            return this.#fs.utimes(this.#ctx("futimes"), this.path, atimeMs, mtimeMs);
        });
    }
    /**
     * A stream over this fd.
     *
     * Serves the handle's own buffer when it is dirty, because those bytes are the file as far
     * as this fd is concerned and the backend has not seen them yet — which is exactly the case
     * `createReadStream({ fd })` exists for.
     */
    openRead(range) {
        return this.#submit("read", async () => {
            this.#assertCanRead();
            if (this.#dirty && this.#buffer)
                return streamOfBytes(this.#buffer, range);
            return this.#fs.openRead(this.#ctx("read"), this.path, range);
        });
    }
}
class HandleRegistry {
    #fs;
    #handles = new Map();
    #nextFd = FIRST_FD;
    constructor(fs) {
        this.#fs = fs;
    }
    get size() {
        return this.#handles.size;
    }
    /**
     * `open`, including the create/truncate/exclusive semantics of the flags.
     *
     * `hasNativeRange` decides the read strategy for the fd's lifetime: with a real positioned
     * read the handle keeps a byte-range cache, and without one it buffers the whole file once.
     * Getting that wrong in the over-claiming direction is quadratic — a derived range slices a
     * fresh whole-file read, so a positioned-read loop would re-read the file per chunk.
     */
    async open(path, flags, hasNativeRange, owner) {
        if (this.#handles.size >= MAX_OPEN) {
            throw fsError("EMFILE", { syscall: "open", path });
        }
        const c = { syscall: "open", reportPath: path };
        let existing;
        try {
            existing = await this.#fs.stat(c, path);
        }
        catch (err) {
            const code = err.code;
            if (code !== "ENOENT" && code !== "ENOTDIR")
                throw err;
        }
        if (flags.exclusive && existing)
            throw fsError("EEXIST", { syscall: "open", path });
        if (!existing && !flags.create)
            throw fsError("ENOENT", { syscall: "open", path });
        let emptied = false;
        if ((!existing && flags.create) || (existing && flags.truncateOnOpen)) {
            await this.#fs.writeFile(c, path, new Uint8Array(0));
            emptied = true;
        }
        const fd = this.#nextFd++;
        const handle = new Handle(fd, path, flags, this.#fs, hasNativeRange, owner);
        if (emptied)
            handle.seedEmptied();
        else
            handle.seedExisting(existing?.size ?? 0);
        this.#handles.set(fd, handle);
        return { fd, entry: existing };
    }
    get(fd, syscall) {
        const handle = this.#handles.get(fd);
        if (!handle || handle.closed)
            throw fsError("EBADF", { syscall });
        return handle;
    }
    /**
     * Flush whatever is buffered for `path`, so an operation addressing the file *by path* sees
     * bytes that were written through an open fd.
     *
     * Without this the buffering above is observable as data loss. A program writes to an fd and
     * then reads the same file by path — a log written on one descriptor and tailed by name is the
     * common shape — and gets the pre-write contents, with the write having reported success and
     * `fstat(fd)` even confirming the new size. `copyFile` and `rename` are worse: they would move
     * the stale bytes and the later flush would then write over the result.
     *
     * Writes stay batched for the case the buffer exists to serve — a run of small writes with
     * nothing else looking at the file — because this only does work when there is a dirty handle
     * for the exact path being asked about.
     */
    async flushPath(path) {
        let pending;
        for (const handle of this.#handles.values()) {
            if (handle.closed || !handle.dirty || handle.path !== path)
                continue;
            (pending ??= []).push(handle.sync());
        }
        if (pending)
            await Promise.all(pending);
    }
    async close(fd) {
        const handle = this.#handles.get(fd);
        if (!handle)
            throw fsError("EBADF", { syscall: "close" });
        // Removed up front, so the fd stops resolving the moment close is requested — node
        // throws EBADF for a closed fd rather than queueing behind the flush.
        this.#handles.delete(fd);
        await handle.close();
    }
    /**
     * Drop the handles one session holds, or every handle when no session is named.
     *
     * The reason this exists at all: a handle now lives on the host and outlives the worker that
     * opened it, so without an explicit teardown at `exit`/`terminate`/`pagehide` every run
     * leaks its open files — and for a memory mount that means leaking the contents of unlinked
     * files, which are kept alive on purpose for exactly as long as a handle refers to them.
     *
     * The `owner` argument is what makes a shared `NodeVfs` survive a short-lived worker. Several
     * workers may run against one filesystem — that is explicitly allowed, and it is the whole
     * shape of a page that spawns a worker per child process — and one of them terminating must
     * not close descriptors the others are still reading. Clearing the map unconditionally, which
     * is what this did, presents as an unrelated later command failing EBADF partway through.
     *
     * A handle with no owner is left alone by a scoped call: nothing opens one today, and leaking
     * it is a great deal cheaper than closing a descriptor that belongs to somebody else.
     *
     * Buffers are **not** flushed. A worker that died did not ask for its dirty writes to land,
     * and inventing a flush would publish half-written files no program asked to publish.
     */
    closeAll(owner) {
        if (owner === undefined) {
            this.#handles.clear();
            return;
        }
        for (const [fd, handle] of this.#handles) {
            if (handle.owner === owner)
                this.#handles.delete(fd);
        }
    }
}

// An in-memory filesystem.
//
// A real tree — directories, mtimes, listings — not a flat path→bytes map, because the
// things built on it need to be indistinguishable from files: `statSync`, `readdirSync`
// of the parent, and the module resolver's own probing all have to work without knowing a
// path is synthetic.
//
// ## The lazy-content seam
//
// `MemFile.content` may be bytes *or* a thunk. That is the hook an archive-backed mount
// hangs on: it builds the tree from the zip's central directory alone — name, size,
// offset — and each file's thunk inflates its own entry on demand, with `size` coming
// from the header so **`stat` never materializes anything** (and the module resolver stats
// far more paths than it reads).
//
// Note what changed in moving here: the thunk used to have to be *synchronous*, because
// the same code had to satisfy `readFileSync` over a blocking transport and could not
// await. Host-side it can return a promise, so `DecompressionStream` is usable and the
// zlib wasm the worker bundle carries is not needed at all. The half of the rationale
// that survives is the valuable half — `stat` staying free.
function now() {
    return Date.now();
}
function newDir() {
    const t = now();
    return {
        kind: "dir",
        children: new Map(),
        mtimeMs: t,
        ctimeMs: t,
        atimeMs: t,
    };
}
function newFile(content, size) {
    const t = now();
    return {
        kind: "file",
        size: size ?? (content instanceof Uint8Array ? content.length : 0),
        mtimeMs: t,
        ctimeMs: t,
        atimeMs: t,
        content,
    };
}
/** Materialize a file's bytes, collapsing a thunk on first use. */
async function bytesOf(file) {
    if (typeof file.content === "function") {
        const produced = await file.content();
        file.content = produced;
        file.size = produced.length;
    }
    return file.content;
}
function segments$1(path) {
    return path.split("/").filter((s) => s.length > 0 && s !== ".");
}
/**
 * The capacity `statfs` declares for a memory mount.
 *
 * Nothing can measure the real ceiling — it is the tab's heap, and no API reports how much of that
 * is available. 1 GiB is chosen to be larger than anything a scratch mount plausibly holds while
 * staying a believable figure for a filesystem, so a caller sizing a write against `free` gets a
 * sane answer instead of a zero.
 */
const MEMORY_CAPACITY = 1024 ** 3;
function createMemoryProvider(opts = {}) {
    const name = opts.name ?? "memory";
    const prefix = opts.prefix ?? "";
    const root = opts.root ?? newDir();
    const events = opts.events ?? NO_EVENTS;
    /** The absolute path a local one corresponds to, for watch events. */
    function full(local) {
        if (!prefix)
            return local;
        return local === "/" ? prefix : prefix + local;
    }
    function lookup(path) {
        let node = root;
        for (const seg of segments$1(path)) {
            if (node.kind !== "dir")
                return undefined;
            const next = node.children.get(seg);
            if (!next)
                return undefined;
            node = next;
        }
        return node;
    }
    function mustFind(path, ctx) {
        const node = lookup(path);
        if (!node)
            throw fsError("ENOENT", ctx);
        return node;
    }
    function mustFile(path, ctx) {
        const node = mustFind(path, ctx);
        if (node.kind === "dir")
            throw fsError("EISDIR", ctx);
        return node;
    }
    /** The containing directory and final segment, for a mutation. */
    function parentOf(path, ctx) {
        const parts = segments$1(path);
        // The mount root itself is not removable or replaceable.
        if (parts.length === 0)
            throw fsError("EPERM", ctx);
        const name = parts[parts.length - 1];
        let node = root;
        for (const seg of parts.slice(0, -1)) {
            if (node.kind !== "dir")
                throw fsError("ENOTDIR", ctx);
            const next = node.children.get(seg);
            if (!next)
                throw fsError("ENOENT", ctx);
            node = next;
        }
        if (node.kind !== "dir")
            throw fsError("ENOTDIR", ctx);
        return { dir: node, name };
    }
    function entryFor(local, entryName, node) {
        return {
            // A *local* path. The facade re-roots it onto the mount before anything above
            // sees it, the same way it does for every provider.
            path: local,
            name: entryName,
            uid: "",
            isDir: node.kind === "dir",
            isSymlink: false,
            size: node.kind === "file" ? node.size : 0,
            modifiedMs: node.mtimeMs,
            createdMs: node.ctimeMs,
            accessedMs: node.atimeMs,
        };
    }
    function mkdirp(path) {
        let node = root;
        for (const seg of segments$1(path)) {
            let next = node.children.get(seg);
            if (!next) {
                next = newDir();
                node.children.set(seg, next);
                node.mtimeMs = now();
            }
            if (next.kind !== "dir") {
                throw fsError("ENOTDIR", { syscall: "mkdir", path });
            }
            node = next;
        }
        return node;
    }
    /** "/", "" and "/a/" all normalize to the form `walk` concatenates against. */
    function normalizeLocal(path) {
        const parts = segments$1(path);
        return parts.length === 0 ? "/" : "/" + parts.join("/");
    }
    // The `list` counterpart to `collect`. Kept separate rather than generalized:
    // `collect` answers readdir (FsEntry, capped depth) and this answers the host
    // (MemListEntry, uncapped, mtime-filtered). Folding them together would mean a
    // function whose every parameter exists for only one of its two callers.
    //
    // `since` filters the output, not the traversal: a directory's mtime does not
    // propagate up from its descendants, so an unmodified ancestor tells you nothing about
    // what changed underneath it and the walk has to be complete regardless.
    function walk(dir, base, out, recursive, since) {
        for (const [childName, child] of dir.children) {
            const childPath = base === "/" ? `/${childName}` : `${base}/${childName}`;
            if (since === undefined || child.mtimeMs > since) {
                out.push({
                    path: childPath,
                    kind: child.kind,
                    size: child.kind === "file" ? child.size : 0,
                    mtimeMs: child.mtimeMs,
                });
            }
            if (recursive && child.kind === "dir") {
                walk(child, childPath, out, recursive, since);
            }
        }
    }
    function collect(dir, base, out, recursive, depth) {
        for (const [childName, child] of dir.children) {
            const childPath = base === "/" ? `/${childName}` : `${base}/${childName}`;
            out.push(entryFor(childPath, childName, child));
            if (recursive && child.kind === "dir" && depth > 1) {
                collect(child, childPath, out, recursive, depth - 1);
            }
        }
    }
    const provider = {
        name,
        root,
        put(path, content, size) {
            const parts = segments$1(path);
            const dir = mkdirp("/" + parts.slice(0, -1).join("/"));
            const file = newFile(content, size);
            dir.children.set(parts[parts.length - 1], file);
            dir.mtimeMs = now();
            return file;
        },
        mkdirp,
        drop(path) {
            const parts = segments$1(path);
            if (parts.length === 0)
                return false;
            const parent = lookup("/" + parts.slice(0, -1).join("/"));
            if (!parent || parent.kind !== "dir")
                return false;
            const removed = parent.children.delete(parts[parts.length - 1]);
            if (removed)
                parent.mtimeMs = now();
            return removed;
        },
        get(path) {
            const node = lookup(path);
            if (!node || node.kind !== "file")
                return undefined;
            if (typeof node.content === "function") {
                throw new Error(`${path} is lazily backed and cannot be read synchronously; read it through the filesystem instead`);
            }
            // No atime bump: a host read is out-of-band inspection, not the program
            // touching its own file.
            return node.content;
        },
        list(path, listOpts) {
            const node = lookup(path);
            if (!node || node.kind !== "dir")
                return undefined;
            const out = [];
            walk(node, normalizeLocal(path), out, !!listOpts?.recursive, listOpts?.since);
            return out;
        },
        async stat(ctx, path) {
            const node = mustFind(path, ctx);
            return entryFor(path, basename(path) || "/", node);
        },
        /**
         * `statfs`, and answering with a real capacity is the whole point.
         *
         * Without this the facade falls back to `{ used: 0, capacity: 0 }`, which reads as a
         * filesystem with **zero bytes free** — so a program that checks for room before writing
         * concludes it has none. Not hypothetical: `/tmp` is a memory mount, Claude Code pre-flights
         * free space on its temp directory before every Bash command, and the zero made *every*
         * command fail with "the temp filesystem is full (0MB free)" while writes to that very
         * directory were succeeding.
         *
         * `used` is summed from the tree — `MemFile.size` is known without materializing lazy
         * content, so this is an in-memory walk with no I/O. `capacity` is a declared budget rather
         * than a measurement: the real ceiling is the tab's heap, which nothing here can query, and
         * this number's job is to be a plausible non-zero denominator with visible headroom. It
         * grows if the contents ever approach it, so `free` never reaches zero and starves a caller
         * that is only asking whether it may proceed.
         */
        async statfs() {
            let used = 0;
            const walk = (dir) => {
                for (const child of dir.children.values()) {
                    if (child.kind === "dir")
                        walk(child);
                    else
                        used += child.size;
                }
            };
            walk(root);
            return { used, capacity: Math.max(MEMORY_CAPACITY, used * 2) };
        },
        async readdir(ctx, path, o) {
            const node = mustFind(path, ctx);
            if (node.kind !== "dir")
                throw fsError("ENOTDIR", ctx);
            const out = [];
            collect(node, path, out, !!o?.recursive, o?.depth ?? Infinity);
            // An in-memory listing is exhaustive by construction — there is no paging and
            // nothing to truncate, so negative inference above is always sound.
            return { entries: out, complete: true };
        },
        // Both reads copy.
        //
        // Handing back the stored bytes would be free, and wrong: `fs.readFileSync`
        // promises a fresh buffer, and callers do mutate what they get — decoders and
        // parsers work in place all the time. Aliasing means such a caller silently
        // rewrites the file it just read, with no write call anywhere. A network backend
        // never has this problem because every read materializes a new buffer from the
        // response, so the hazard is unique to serving bytes out of memory.
        //
        // `subarray` is a view, not a copy, so the ranged read needs the same treatment.
        async readFile(ctx, path) {
            const node = mustFile(path, ctx);
            node.atimeMs = now();
            return new Uint8Array(await bytesOf(node));
        },
        async readRange(ctx, path, offset, length) {
            const node = mustFile(path, ctx);
            return new Uint8Array((await bytesOf(node)).subarray(offset, offset + length));
        },
        async openRead(ctx, path, range) {
            return streamOfBytes(await bytesOf(mustFile(path, ctx)), range);
        },
        async writeFile(ctx, path, data) {
            const { dir, name: base } = parentOf(path, ctx);
            const existing = dir.children.get(base);
            if (existing && existing.kind === "dir")
                throw fsError("EISDIR", ctx);
            // A copy: what arrives is a view into the request frame, which is not ours.
            const copy = new Uint8Array(data);
            if (existing) {
                existing.content = copy;
                existing.size = copy.length;
                existing.mtimeMs = now();
            }
            else {
                dir.children.set(base, newFile(copy));
                dir.mtimeMs = now();
            }
            // Millisecond resolution, unlike puterfs's one-second timestamps — so a
            // watcher can tell two writes in the same second apart.
            events.write(full(path));
        },
        async mkdir(ctx, path, o) {
            if (o.recursive) {
                mkdirp(path);
                events.add(full(path), true);
                return undefined;
            }
            const { dir, name: base } = parentOf(path, ctx);
            if (dir.children.has(base))
                throw fsError("EEXIST", ctx);
            dir.children.set(base, newDir());
            dir.mtimeMs = now();
            events.add(full(path), true);
            return undefined;
        },
        async rm(ctx, path, o) {
            const parts = segments$1(path);
            if (parts.length === 0)
                throw fsError("EPERM", ctx);
            const parent = lookup("/" + parts.slice(0, -1).join("/"));
            if (!parent || parent.kind !== "dir") {
                if (o.force)
                    return;
                throw fsError("ENOENT", ctx);
            }
            const base = parts[parts.length - 1];
            const node = parent.children.get(base);
            if (!node) {
                if (o.force)
                    return;
                throw fsError("ENOENT", ctx);
            }
            if (node.kind === "dir" && node.children.size > 0 && !o.recursive) {
                throw fsError("ENOTEMPTY", ctx);
            }
            // Detach but leave the node intact: an open handle keeps reading it, and its
            // flush is suppressed rather than resurrecting the file.
            if (node.kind === "file")
                node.unlinked = true;
            parent.children.delete(base);
            parent.mtimeMs = now();
            events.remove(full(path), node.kind === "dir");
        },
        async rename(ctx, from, to) {
            const src = parentOf(from, ctx);
            const node = src.dir.children.get(src.name);
            if (!node)
                throw fsError("ENOENT", ctx);
            const dst = parentOf(to, ctx);
            // `rename(2)` replaces the destination, but only where that loses nothing. Without
            // these checks a directory renamed over a *non-empty* directory silently discarded the
            // whole subtree underneath it — which is data loss dressed as a successful call, and
            // what node reports instead is ENOTEMPTY.
            const existing = dst.dir.children.get(dst.name);
            if (existing && existing !== node) {
                const intoDir = existing.kind === "dir";
                if (node.kind === "dir" && !intoDir)
                    throw fsError("ENOTDIR", ctx);
                if (node.kind !== "dir" && intoDir)
                    throw fsError("EISDIR", ctx);
                if (intoDir && existing.children.size > 0) {
                    throw fsError("ENOTEMPTY", ctx);
                }
            }
            src.dir.children.delete(src.name);
            dst.dir.children.set(dst.name, node);
            src.dir.mtimeMs = now();
            dst.dir.mtimeMs = now();
            events.move(full(from), full(to), node.kind === "dir");
        },
        async copyFile(ctx, from, to, o) {
            const node = mustFile(from, ctx);
            const dst = parentOf(to, ctx);
            if (dst.dir.children.has(dst.name) && !o.overwrite) {
                throw fsError("EEXIST", ctx);
            }
            dst.dir.children.set(dst.name, newFile(new Uint8Array(await bytesOf(node))));
            dst.dir.mtimeMs = now();
            events.add(full(to));
        },
        // Real timestamps, exactly as asked — nothing here is limited to "now" the way
        // puterfs's `/touch` is.
        async utimes(ctx, path, atimeMs, mtimeMs) {
            const node = mustFind(path, ctx);
            node.atimeMs = atimeMs;
            node.mtimeMs = mtimeMs;
            events.write(full(path));
            return true;
        },
    };
    return provider;
}

// The puter api, from the page.
//
// This used to live in the worker and reach the api with two transports: `fetch` for the
// async fs surface and a *blocking* `XMLHttpRequest` for the synchronous one. Only the
// first survives — the blocking transport was never about puterfs, it was about
// `readFileSync`, and that need is served by the service-worker bridge now. So there is one
// transport here, and it is an ordinary `fetch`.
//
// Worth noting that nothing about how these calls reach the network changed: the worker
// deliberately used the *native* pre-proxy `fetch` snapshot (epoxy replaces
// `globalThis.fetch` with a WISP tunnel for user code), so these requests were already
// going out the browser's own stack.
const DEFAULT_API_ORIGIN = "https://api.puter.com";
function getRandomId() {
    return [...Array(16)].reduce((a) => a + Math.random().toString(36)[2], "");
}
// -------------------------------------------------------------- entry shapes
/**
 * puterfs timestamps are unix *seconds*. Missing or garbage becomes 0 (the epoch) rather
 * than NaN: node's stat never yields an Invalid Date, and a NaN here silently poisons
 * every `mtime` comparison downstream.
 */
function toMs(v) {
    const n = Number(v);
    return Number.isFinite(n) ? n * 1000 : 0;
}
/**
 * Accepts either wire shape:
 *   - v2 camelCase, from `/fs/readdir` (`isDir`, `modified`, …)
 *   - v1 snake_case, from the legacy `/stat` and `/readdir` routes (`is_dir`, and
 *     `is_symlink` as an int 0|1)
 *
 * Neither has ever had `created_at`/`updated_at`, despite what this runtime used to read —
 * the fields are `created`/`modified`/`accessed`.
 */
function normalizeFsEntry(raw) {
    return {
        path: raw.path,
        name: raw.name,
        uid: raw.uid ?? raw.uuid ?? raw.id,
        isDir: Boolean(raw.isDir ?? raw.is_dir),
        isSymlink: Boolean(raw.isSymlink ?? raw.is_symlink),
        size: Number(raw.size ?? 0),
        modifiedMs: toMs(raw.modified),
        createdMs: toMs(raw.created),
        accessedMs: toMs(raw.accessed),
    };
}
/**
 * The request body every `stat` call sends.
 *
 * `return_size` is deliberately absent. It only does anything for directories, where the
 * backend answers it with `SUM(size)` over the entire subtree — an O(descendants) index
 * scan — so a single `statSync` on a project root makes the server walk all of
 * node_modules. node reports a directory's `Stats.size` as a block count, never a subtree
 * total, so the field was never usable anyway.
 */
function statRequest(path) {
    return {
        path,
        return_permissions: false,
        return_versions: false,
        consistency: "strong",
    };
}
/** Monotonic per-request token for the `_` parameter on cacheable GETs. */
let cacheBuster = 0;
function cacheBust() {
    return String(cacheBuster++);
}
/**
 * The url every file read GETs, cache-busted.
 *
 * The api answers `/read` with `ETag` and `Last-Modified` but *no* `Cache-Control`, which
 * is precisely the case where a browser is allowed to invent its own freshness lifetime
 * (RFC 9111 heuristic caching) and serve the body out of the disk cache without
 * revalidating. The bytes on disk then outlive the file: rewrite it and the next read still
 * returns the old version, which is what breaks HMR.
 *
 * `_` is inert on the server: the legacy `/read` handler dispatches on `file` alone. This
 * stays a CORS-simple GET, so it costs no preflight — unlike a `Cache-Control: no-cache`
 * request header, which would.
 */
function readUrl(path) {
    return `read?file=${encodeURIComponent(path)}&_=${cacheBust()}`;
}
// ----------------------------------------------------------- error mapping
/**
 * Puter api error codes → node errno names. Codes are defined in the backend at
 * `src/backend/src/api/APIError.js`.
 *
 * Only the node code is recorded; the errno and the bare message come from `ERRNO` in
 * ../../vfs/errno.ts, so those facts exist once.
 */
const PUTER_TO_NODE = {
    subject_does_not_exist: "ENOENT",
    source_does_not_exist: "ENOENT",
    dest_does_not_exist: "ENOENT",
    shortcut_target_not_found: "ENOENT",
    offset_without_existing_file: "ENOENT",
    item_with_same_name_exists: "EEXIST",
    forbidden: "EACCES",
    permission_denied: "EACCES",
    immutable: "EACCES",
    not_empty: "ENOTEMPTY",
    dest_is_not_a_directory: "ENOTDIR",
    readdir_of_non_directory: "ENOTDIR",
    cannot_read_a_directory: "EISDIR",
    cannot_overwrite_a_directory: "EISDIR",
    invalid_file_name: "EINVAL",
    unresolved_relative_path: "EINVAL",
    invalid_operation: "EINVAL",
    // The api's catch-all for a malformed request. Reachable from normal code: a recursive
    // readdir of `/` returns it (see ./puter-readdir.ts).
    bad_request: "EINVAL",
    cannot_move_item_into_itself: "EINVAL",
    cannot_copy_item_into_itself: "EINVAL",
    source_and_dest_are_the_same: "EINVAL",
    cannot_move_to_root: "EPERM",
    cannot_copy_to_root: "EPERM",
    cannot_write_to_root: "EPERM",
    storage_limit_reached: "ENOSPC",
    file_too_large: "EFBIG",
    not_yet_supported: "ENOTSUP",
    missing_filesystem_capability: "ENOTSUP",
};
/**
 * Turn a non-`ok` response body into the node error for it. Every failure path in the
 * provider goes through here, so the mapping lives in one place.
 *
 * An unrecognized code becomes EIO rather than an uncoded `Error`: node's `fs` never throws
 * without a code, and this tree is full of `catch (e) { if (e.code !== "ENOENT") throw e }`.
 */
function failPuter(body, ctx) {
    const code = PUTER_TO_NODE[body?.code];
    if (code)
        throw fsError(code, ctx);
    throw fsError("EIO", {
        ...ctx,
        message: body?.message ?? `${ctx.syscall} failed on ${ctx.reportPath}`,
    });
}
// --------------------------------------------------------------- rate limiting
/**
 * The one status this retries, and the reason it is the only one.
 *
 * A 429 says the request was *refused without being performed*, which is what makes replaying
 * it safe — including for a `POST /write` or `/move`, since nothing in this api is idempotent
 * and nothing here can make it so. A 502 or 504 says nothing about whether the mutation
 * landed, so retrying one could duplicate it; those are reported as they arrive.
 */
const THROTTLED = 429;
/** One original attempt plus this many retries. */
const RETRY_ATTEMPTS = 3;
const RETRY_BASE_MS = 300;
const RETRY_MAX_MS = 4000;
const RETRY_JITTER = 0.5;
/**
 * Shortest pause a 429 can buy, however little the server asked for.
 *
 * `Retry-After` as an http-date has *second* granularity, so "half a second from now" is sent
 * as a timestamp that has already passed and parses as a zero wait — which would turn the
 * retries into a tight loop against a server that just said it was overloaded. Only ever binds
 * on a `Retry-After`; the exponential path starts an order of magnitude above it.
 */
const RETRY_MIN_MS = 50;
/**
 * Total time one call will spend waiting before it gives up and reports the 429.
 *
 * Most of these have a caller parked in a *synchronous* `readdirSync` behind them, so the wait
 * is a frozen worker rather than an idle promise. A `Retry-After` longer than what is left of
 * this budget is honoured by giving up rather than by sleeping through it.
 */
const RETRY_BUDGET_MS = 8000;
/** Counter keys that are not endpoints. Parenthesized so they cannot collide with a path. */
const THROTTLE_KEY = "(429 retried)";
const THROTTLE_GAVE_UP_KEY = "(429 gave up)";
/**
 * `Retry-After`, in milliseconds, when the server sent one *and* we are allowed to read it.
 *
 * Usually neither. It is not a CORS-safelisted response header, so on a cross-origin reply it
 * is invisible here unless the backend names it in `Access-Control-Expose-Headers` — which is
 * why the exponential fallback below is the path that actually runs, not the edge case.
 */
function retryAfterMs(res) {
    const raw = res.headers.get("Retry-After");
    if (!raw)
        return undefined;
    const seconds = Number(raw);
    if (Number.isFinite(seconds))
        return Math.max(0, seconds * 1000);
    const when = Date.parse(raw);
    return Number.isFinite(when) ? Math.max(0, when - Date.now()) : undefined;
}
function backoffMs(attempt) {
    const base = Math.min(RETRY_BASE_MS * 2 ** attempt, RETRY_MAX_MS);
    // Jittered because these arrive in bursts: a directory walk has dozens of requests in
    // flight, and an unjittered backoff would have all of them return at the same instant and
    // reproduce the burst that earned the 429.
    return base * (1 + RETRY_JITTER * (Math.random() * 2 - 1));
}
function sleep(ms, signal) {
    return new Promise((resolve, reject) => {
        if (signal?.aborted) {
            reject(signal.reason);
            return;
        }
        const onAbort = () => {
            clearTimeout(timer);
            reject(signal.reason);
        };
        const timer = setTimeout(() => {
            signal?.removeEventListener("abort", onAbort);
            resolve();
        }, ms);
        signal?.addEventListener("abort", onAbort, { once: true });
    });
}
const decoder = new TextDecoder("utf-8");
function makeResponse(ok, status, bytes) {
    let decoded;
    let tried = false;
    return {
        ok,
        status,
        bytes,
        json() {
            if (!tried) {
                tried = true;
                try {
                    decoded = JSON.parse(decoder.decode(bytes));
                }
                catch {
                    decoded = undefined;
                }
            }
            return decoded;
        },
    };
}
function handleBody(bodyInit) {
    if (!bodyInit)
        return undefined;
    if (bodyInit instanceof Function) {
        const form = new FormData();
        bodyInit(form);
        return form;
    }
    return JSON.stringify(bodyInit);
}
class PuterApi {
    #token;
    #origin;
    /**
     * Per-endpoint call counts, keyed by the path with the query string stripped. The
     * whole point of the resolver and readdir work is to make this number go down, and
     * there is no other way to see it — it rides back to the worker on the reply frame so
     * `NODE_WORKER_API_STATS` keeps reporting it after the move.
     */
    #counts = new Map();
    /**
     * When the last 429 said it was worth asking again. Shared across every call, since the
     * limit is on the account rather than on any one request.
     */
    #cooldownUntil = 0;
    /** Snapshot of `#counts` at the last `drainStats`, so the next one reports a delta. */
    #drained;
    constructor(token, origin = DEFAULT_API_ORIGIN) {
        this.#token = token;
        this.#origin = origin;
    }
    get origin() {
        return this.#origin;
    }
    stats() {
        return Object.fromEntries([...this.#counts].sort((a, b) => b[1] - a[1]));
    }
    resetStats() {
        this.#counts.clear();
    }
    /**
     * Counts since the last drain, and reset.
     *
     * The per-reply sideband needs a *delta*: it reports what one call cost, so handing it
     * the cumulative total would make every reply restate the whole run. `stats()` stays
     * cumulative for `apiStats()`, which is a different question — what has this page sent
     * altogether.
     */
    drainStats() {
        if (this.#drained === undefined)
            this.#drained = new Map();
        const out = {};
        let any = false;
        for (const [key, total] of this.#counts) {
            const delta = total - (this.#drained.get(key) ?? 0);
            if (delta <= 0)
                continue;
            out[key] = delta;
            any = true;
        }
        if (!any)
            return undefined;
        this.#drained = new Map(this.#counts);
        return out;
    }
    /**
     * A GET with no custom request headers is a CORS-*simple* request, so the browser skips
     * the preflight entirely. Carrying the token as `?auth_token=` instead of an
     * `Authorization` header is what keeps it simple (the api accepts either), and it halves
     * the round trips. POSTs always send `Content-Type: application/json`, which preflights
     * regardless, so they keep the header.
     */
    #url(path, method, headers) {
        const url = new URL(`${this.#origin}/${path}`);
        if (method === "GET")
            url.searchParams.append("auth_token", this.#token);
        else
            headers["Authorization"] = "Bearer " + this.#token;
        return url.toString();
    }
    async fetch(path, bodyInit, signal, extraHeaders) {
        const method = bodyInit ? "POST" : "GET";
        const headers = bodyInit && !(bodyInit instanceof Function)
            ? { "Content-Type": "application/json" }
            : {};
        if (extraHeaders)
            Object.assign(headers, extraHeaders);
        const res = await this.#send(path, this.#url(path, method, headers), { headers, method, body: handleBody(bodyInit), signal }, signal);
        return makeResponse(res.ok, res.status, new Uint8Array(await res.arrayBuffer()));
    }
    /**
     * Like `fetch`, but hands back the un-consumed `Response` so the caller can read the
     * body incrementally. `openRead` uses this to serve a whole file from one request
     * instead of a ranged GET per chunk.
     */
    async fetchStream(path, signal, extraHeaders) {
        const headers = {};
        if (extraHeaders)
            Object.assign(headers, extraHeaders);
        return this.#send(path, this.#url(path, "GET", headers), { headers, method: "GET", signal }, signal);
    }
    /**
     * One request, with 429 retried. Everything in this class goes through here, the streaming
     * read included, because a rate limit is about the account and not about the endpoint.
     */
    async #send(path, url, init, signal) {
        const deadline = Date.now() + RETRY_BUDGET_MS;
        for (let attempt = 0;; attempt++) {
            await this.#awaitCooldown(deadline, signal);
            this.#count(path);
            const res = await fetch(url, init);
            if (res.status !== THROTTLED)
                return res;
            const wait = Math.max(RETRY_MIN_MS, retryAfterMs(res) ?? backoffMs(attempt));
            if (attempt >= RETRY_ATTEMPTS || Date.now() + wait > deadline) {
                this.#bump(THROTTLE_GAVE_UP_KEY);
                return res;
            }
            this.#bump(THROTTLE_KEY);
            // The limit is the account's, not this request's, so hold the others back too.
            // Without it every call already in flight spends its own attempts rediscovering
            // the same limit, and the retries are the burst all over again.
            this.#cooldownUntil = Math.max(this.#cooldownUntil, Date.now() + wait);
            // The body is a rejection notice nobody reads, and leaving it unconsumed keeps
            // the connection checked out.
            res.body?.cancel().catch(() => { });
            await sleep(wait, signal);
        }
    }
    /**
     * Wait out a cooldown another request's 429 established.
     *
     * Capped by our own deadline: past it there is nothing left to gain by waiting, so the
     * request goes out and its 429, if it comes, is reported to the caller.
     */
    async #awaitCooldown(deadline, signal) {
        const wait = Math.min(this.#cooldownUntil - Date.now(), deadline - Date.now());
        if (wait > 0)
            await sleep(wait, signal);
    }
    #count(path) {
        this.#bump(path.split("?")[0]);
    }
    #bump(key) {
        this.#counts.set(key, (this.#counts.get(key) ?? 0) + 1);
    }
}

// Directory listing against the api's `/fs/readdir` route, which can return a whole
// subtree in one paged call (`recursive` + `depth`).
//
// The paging, depth-horizon and error handling live here rather than being written out at
// each call site — this was the first operation to get that treatment, back when it had to
// be a generator so the sync and async fs surfaces could share it. Being ordinary async
// code now costs it nothing: the *reason* for one copy was never the transport, it was that
// this logic is fiddly enough to drift if duplicated.
/**
 * The api caps `limit` at 10k and defaults to 1k. Responses are parsed with a single
 * non-streaming `JSON.parse`, so oversized pages cost a big transient string; 5k is a
 * compromise between that and the per-request round trip.
 */
const PAGE_LIMIT = 5000;
function readdirUrl(path, opts) {
    const q = new URLSearchParams();
    q.set("path", path);
    if (opts.recursive) {
        q.set("recursive", "true");
        q.set("depth", String(opts.depth ?? MAX_DEPTH));
    }
    if (opts.includeTotal)
        q.set("includeTotal", "true");
    q.set("limit", String(PAGE_LIMIT));
    // Always present, empty on the first page. Sending `cursor` at all is what opts into
    // the `{items, cursor?}` envelope; without it a non-recursive listing comes back as a
    // bare array that the server has already truncated to `limit` with no way to ask for
    // the rest.
    q.set("cursor", opts.cursor ?? "");
    // Cache-busted for the same reason `readUrl` is: a silently stale listing is a
    // miserable bug to chase and it costs one query parameter.
    q.set("_", cacheBust());
    // GET rather than POST: it carries the token as `?auth_token=`, which keeps it a
    // CORS-simple request and skips the preflight.
    return `fs/readdir?${q}`;
}
/**
 * A recursive response is always the `{items, cursor?, total?}` envelope; the non-recursive
 * form is a bare array. Both reduce to this.
 */
function toPage(body) {
    if (Array.isArray(body)) {
        return { items: body, cursor: undefined, total: undefined };
    }
    return {
        items: body?.items ?? [],
        cursor: body?.cursor ?? undefined,
        total: typeof body?.total === "number" ? body.total : undefined,
    };
}
/**
 * List one directory, following the cursor to the end. With `recursive` this is the whole
 * subtree down to `depth` (default and maximum {@link MAX_DEPTH}), excluding the root
 * itself.
 */
async function readdirPages(api, ctx, root, opts = {}) {
    const maxEntries = opts.maxEntries ?? Infinity;
    const entries = [];
    let cursor;
    let first = true;
    do {
        const res = await api.fetch(readdirUrl(root, {
            recursive: opts.recursive,
            depth: opts.depth,
            cursor,
            // Ask the server to count the subtree on the first page whenever a budget is
            // in play, so an oversized listing is abandoned after one page instead of
            // after `maxEntries`-worth of them. Costs one COUNT(*) and no extra round
            // trip.
            includeTotal: first && maxEntries !== Infinity,
        }));
        const body = res.json();
        if (!res.ok)
            failPuter(body, ctx);
        const page = toPage(body);
        if (first && page.total !== undefined && page.total > maxEntries) {
            return { entries, complete: false };
        }
        first = false;
        for (const item of page.items)
            entries.push(normalizeFsEntry(item));
        cursor = page.cursor;
        if (entries.length >= maxEntries)
            return { entries, complete: false };
        // Only a null/absent cursor means "last page" — `items.length < limit` does not,
        // and treating it as such truncates listings at random.
    } while (cursor !== undefined && cursor !== null);
    return { entries, complete: true };
}
/**
 * Every descendant of `root`, at any depth.
 *
 * The api caps a single recursive call at {@link MAX_DEPTH} levels, so any directory
 * returned at exactly that depth becomes a new root and is walked again. No de-duplication
 * is needed: a call rooted at `r` excludes `r` itself, so a follow-up rooted at a horizon
 * directory returns a disjoint set.
 *
 * Entries come back ordered by full path, ascending — the api's ordering. node guarantees
 * no particular order.
 */
async function readdirTree(api, ctx, root) {
    const out = [];
    let frontier;
    if (root === "/") {
        // The api refuses a recursive listing at the root (400 bad_request — it would be a
        // prefix scan over every user). Enumerate it flat instead and treat each top-level
        // directory as its own recursive root.
        const page = await readdirPages(api, ctx, "/", { recursive: false });
        out.push(...page.entries);
        frontier = page.entries.filter((e) => e.isDir).map((e) => e.path);
    }
    else {
        frontier = [root];
    }
    while (frontier.length > 0) {
        const current = frontier.shift();
        const page = await readdirPages(api, ctx, current, { recursive: true });
        out.push(...page.entries);
        for (const entry of page.entries) {
            if (entry.isDir && relDepth(current, entry.path) === MAX_DEPTH) {
                frontier.push(entry.path);
            }
        }
    }
    return out;
}

// The puterfs backend.
//
// One copy of each request shape. Nothing here touches node's `Stats`/`Dirent` — those are
// node's shapes and belong to the fs surface inside the worker, while this layer speaks
// `FsEntry`. That is a layering rule and a cycle guard at the same time.
/**
 * The api can only set a timestamp to *now* (`POST /touch` takes `set_modified_to_now` and
 * friends — there is no field for an arbitrary value), so this decides whether a requested
 * time is close enough to now to be worth a round trip. Two seconds covers the gap between
 * a caller reading the clock and us issuing the request, which is what makes `touch(1)`-style
 * callers work.
 */
const TOUCH_NOW_TOLERANCE_MS = 2000;
function isEffectivelyNow(epochMs) {
    return Math.abs(Date.now() - epochMs) <= TOUCH_NOW_TOLERANCE_MS;
}
/**
 * The multipart body for a whole-file write. puterfs has no partial-write primitive —
 * `POST /batch` with `op: "write"` and `overwrite: true` replacing the entire file is the
 * only way to change one — so this is the sole write path, and every caller that looks like
 * an incremental write is buffering to reach it.
 */
function writeBody(path, data) {
    const name = basename(path);
    const parent = dirname(path);
    return (form) => {
        const opId = getRandomId();
        form.append("operation_id", opId);
        form.append("fileinfo", JSON.stringify({
            name,
            type: "application/octet-stream",
            size: data.byteLength,
        }));
        form.append("operation", JSON.stringify({
            op: "write",
            dedupe_name: false,
            overwrite: true,
            operation_id: opId,
            path: parent,
            name,
            item_upload_id: 0,
        }));
        // A fresh copy, because what arrives is a view into the request frame and a `Blob`
        // must not alias a buffer the transport may reuse.
        form.append("file", new File([new Uint8Array(data)], name));
    };
}
function createPuterProvider(opts) {
    const { api } = opts;
    const events = opts.events ?? NO_EVENTS;
    /**
     * mkdir, including the implicitly-created parents a recursive mkdir reports in
     * `parent_dirs_created`. node's watchers see each new directory, so emitting only the
     * leaf would hide the rest of the chain.
     */
    function emitMkdir(path, res) {
        const parents = res?.parent_dirs_created;
        if (Array.isArray(parents)) {
            for (const parent of parents) {
                if (typeof parent === "string" && parent !== path)
                    events.add(parent, true);
            }
        }
        events.add(path, true);
    }
    return {
        name: "puter",
        async stat(ctx, path) {
            const res = await api.fetch("stat", statRequest(path));
            const body = res.json();
            if (!res.ok)
                failPuter(body, ctx);
            return normalizeFsEntry(body);
        },
        async readdir(ctx, path, opts) {
            if (opts?.recursive && opts.depth === undefined) {
                // No depth given means "everything", which needs the re-rooting horizon walk
                // rather than a single capped request.
                return { entries: await readdirTree(api, ctx, path), complete: true };
            }
            return readdirPages(api, ctx, path, {
                recursive: opts?.recursive,
                depth: opts?.depth,
                maxEntries: opts?.maxEntries,
            });
        },
        async readFile(ctx, path) {
            const res = await api.fetch(readUrl(path));
            if (!res.ok)
                failPuter(res.json(), ctx);
            return res.bytes;
        },
        /**
         * Ranged read via the HTTP `Range` header.
         *
         * NOT `?offset=&byte_count=`: the api's `/read` handler ignores those query
         * parameters and answers with the *whole file*, which is worse than an error — a
         * caller reading at a non-zero position would silently get bytes from offset 0, and
         * a stream would never see EOF because every read returns data.
         *
         * The cost is a CORS preflight, since a custom header makes this a non-simple GET.
         * Only positioned reads pay it; whole-file reads go through `readFile`.
         */
        async readRange(ctx, path, offset, length) {
            const end = offset + length - 1;
            const res = await api.fetch(readUrl(path), undefined, undefined, {
                Range: `bytes=${offset}-${end}`,
            });
            if (!res.ok) {
                // 416 means the range starts at or past EOF, which for a positioned read is
                // simply "no bytes there" — what libuv reports as 0.
                if (res.status === 416)
                    return new Uint8Array(0);
                failPuter(res.json(), ctx);
            }
            // A 200 means the server ignored the range and sent everything; slicing keeps
            // this correct if that ever regresses.
            return res.status === 206
                ? res.bytes
                : res.bytes.subarray(offset, offset + length);
        },
        /**
         * A whole file (or a window of one) over a single streamed GET, rather than a ranged
         * read per chunk — the difference between 1 and `ceil(size / highWaterMark)` api
         * calls for a large file.
         *
         * The 416 and 200-vs-206 handling that used to sit in the worker's `ReadStream`
         * belongs here: it is knowledge about *this* backend, and having it upstairs meant
         * every mount paid attention to puterfs's quirks.
         */
        async openRead(ctx, path, range) {
            const ranged = range !== undefined && (range.start !== 0 || range.end !== undefined);
            const headers = ranged
                ? {
                    Range: `bytes=${range.start}-${range.end === undefined ? "" : range.end}`,
                }
                : undefined;
            const res = await api.fetchStream(readUrl(path), undefined, headers);
            if (!res.ok) {
                // 416: `start` is at or past EOF. node's createReadStream yields no data for
                // that rather than erroring.
                if (res.status === 416)
                    return { size: 0, stream: emptyStream() };
                let body;
                try {
                    body = JSON.parse(await res.text());
                }
                catch {
                    body = undefined;
                }
                failPuter(body, ctx);
            }
            if (!res.body)
                return { size: 0, stream: emptyStream() };
            // A 200 for a ranged request means the server ignored the Range; trim so the
            // window is still honored.
            if (ranged && res.status !== 206) {
                const all = new Uint8Array(await res.arrayBuffer());
                const start = range.start;
                const end = range.end === undefined ? all.length : range.end + 1;
                const slice = new Uint8Array(all.subarray(start, end));
                return {
                    size: slice.length,
                    stream: new ReadableStream({
                        start(c) {
                            if (slice.length)
                                c.enqueue(slice);
                            c.close();
                        },
                    }),
                };
            }
            const len = Number(res.headers.get("content-length"));
            return {
                size: Number.isFinite(len) && len >= 0 ? len : undefined,
                stream: res.body,
            };
        },
        async writeFile(ctx, path, data) {
            const res = await api.fetch("batch", writeBody(path, data));
            // `/batch` reports per-operation success in its body and answers 218 when any
            // operation failed, so the transport-level `ok` is not the whole story.
            const result = res.json()?.results?.[0];
            if (result?.success === false)
                failPuter(result, ctx);
            if (!res.ok && !result)
                failPuter(res.json(), ctx);
            // This is the moment the file changes as far as puterfs is concerned.
            events.write(path);
        },
        async mkdir(ctx, path, opts) {
            const res = await api.fetch("mkdir", {
                parent: dirname(path),
                path: basename(path),
                overwrite: opts.recursive,
                dedupe_name: false,
                create_missing_parents: opts.recursive,
            });
            const body = res.json();
            if (!res.ok)
                failPuter(body, ctx);
            emitMkdir(path, body);
            // node returns the first directory created, or undefined. puterfs doesn't
            // reliably report it, so guard rather than throw.
            return opts.recursive ? body?.parent_dirs_created?.[0] : undefined;
        },
        async rm(ctx, path, opts) {
            const res = await api.fetch("delete", {
                paths: [path],
                recursive: opts.recursive,
                descendants_only: false,
            });
            if (!res.ok) {
                // `force` swallows the failure — but nothing was removed, so no event.
                if (opts.force)
                    return;
                failPuter(res.json(), ctx);
            }
            events.remove(path, opts.recursive);
        },
        /**
         * `rename`, including POSIX's replace-the-destination behaviour.
         *
         * This has to replace an existing destination, because write-to-temp-then-rename is how
         * every atomic save in the ecosystem works — `write-file-atomic`, fs-extra, npm, git, and
         * Claude Code's own `.claude.json`, which failed with EEXIST on *every* save while this
         * refused the collision.
         *
         * Puter's `move` only replaces when `overwrite` is set, and its `overwrite` is stronger than
         * rename is allowed to be: it `remove(collision, { recursive: true })`s whatever is in the
         * way. rename must never do that. Replacing a directory with a file is an error, and so is
         * replacing a non-empty directory — not a licence to delete a tree.
         *
         * So the collision is resolved rather than pre-empted: try without `overwrite`, and only if
         * something is actually in the way look at what it is. The common case stays one round trip,
         * and a stat is paid for only when there is a decision to make.
         */
        async rename(ctx, from, to) {
            const move = (overwrite) => api.fetch("move", {
                source: from,
                destination: dirname(to),
                new_name: basename(to),
                overwrite,
                create_missing_parents: false,
            });
            let res = await move(false);
            if (!res.ok && res.json()?.code === "item_with_same_name_exists") {
                const [source, dest] = await Promise.all([
                    api.fetch("stat", statRequest(from)),
                    api.fetch("stat", statRequest(to)),
                ]);
                const sourceIsDir = source.ok && !!normalizeFsEntry(source.json()).isDir;
                const destIsDir = dest.ok && !!normalizeFsEntry(dest.json()).isDir;
                if (destIsDir && !sourceIsDir)
                    throw fsError("EISDIR", ctx);
                if (!destIsDir && sourceIsDir)
                    throw fsError("ENOTDIR", ctx);
                if (destIsDir && sourceIsDir) {
                    // A directory may only take the place of an empty one.
                    const listing = await readdirPages(api, ctx, to, { maxEntries: 1 });
                    if (listing.entries.length > 0)
                        throw fsError("ENOTEMPTY", ctx);
                }
                res = await move(true);
            }
            if (!res.ok)
                failPuter(res.json(), ctx);
            events.move(from, to);
        },
        async copyFile(ctx, from, to, opts) {
            const res = await api.fetch("copy", {
                source: from,
                destination: dirname(to),
                new_name: basename(to),
                overwrite: opts.overwrite,
                dedupe_name: false,
            });
            if (!res.ok)
                failPuter(res.json(), ctx);
            events.add(to);
        },
        /**
         * The only timestamp api is `POST /touch`, whose fields are
         * `set_{modified,accessed,created}_to_now` — there is no field for a value. So
         * "approximately now" is the only representable request, and anything else is
         * reported as not applied rather than faked.
         */
        async utimes(ctx, path, atimeMs, mtimeMs) {
            const setAccessed = isEffectivelyNow(atimeMs);
            const setModified = isEffectivelyNow(mtimeMs);
            if (!setAccessed && !setModified)
                return false;
            const res = await api.fetch("touch", {
                path,
                set_accessed_to_now: setAccessed,
                set_modified_to_now: setModified,
                create_missing_parents: false,
            });
            if (!res.ok)
                failPuter(res.json(), ctx);
            events.write(path);
            return true;
        },
        async statfs(ctx) {
            // `body: {}` rather than omitting it — an empty JSON POST, which is what `/df`
            // expects. Presence of a body is what selects POST over GET.
            const res = await api.fetch("df", {});
            const body = res.json();
            if (!res.ok)
                failPuter(body, ctx);
            return body;
        },
    };
}
function emptyStream() {
    return new ReadableStream({
        start(c) {
            c.close();
        },
    });
}

// Two providers stacked over one subtree.
//
// This is where layering lives, rather than in the mount table — the table stays
// one-provider-per-root and trivially correct, and the "which layer answers" question is a
// single independently testable module.
//
// The shape an archive mount will want: the zip as a read-only *lower* layer with real
// storage as the writable *upper* one, so `node_modules` reads come out of the archive
// while anything a build writes there (vite's dependency optimizer parking pre-bundled
// deps under `node_modules/.vite`) lands in real storage and survives a restart.
//
// ## No whiteouts
//
// There is nowhere to record "this lower-layer path was deleted": puterfs has no such
// concept and a sidecar marker file inside a user's node_modules is worse than the
// limitation. Two consequences, both intended for the archive case and both wrong for a
// general-purpose overlay:
//
//   - deleting an upper file that also exists below makes the lower version reappear;
//   - deleting a lower-only file reports EROFS.
//
// One thing that got *better* in moving host-side: `openRead` can now be forwarded. It
// used to be impossible — picking a layer needs a stat, and `openRead` sat outside the
// generator protocol with no way to perform one — so a union mount could not stream at
// all and `createReadStream` had to fall back. Here it is an ordinary `await`.
async function has(provider, ctx, path) {
    try {
        return await provider.stat(ctx, path);
    }
    catch (err) {
        const code = err.code;
        if (code === "ENOENT" || code === "ENOTDIR")
            return undefined;
        throw err;
    }
}
function unionProvider(upper, lower, write) {
    function roFail(ctx) {
        throw fsError("EROFS", ctx);
    }
    /** Which layer a mutation should go to. */
    async function writeLayer(ctx, path) {
        if (write === "upper")
            return upper;
        if (await has(upper, ctx, path))
            return upper;
        return lower;
    }
    /** Which layer a read should come from. */
    async function readLayer(ctx, path) {
        return (await has(upper, ctx, path)) ? upper : lower;
    }
    const provider = {
        name: `union(${upper.name},${lower.name})`,
        async stat(ctx, path) {
            const up = await has(upper, ctx, path);
            if (!up)
                return lower.stat(ctx, path);
            // A directory present in both layers is *one* directory — the union shows the
            // merged contents either way, so the only question is whose timestamps to
            // report, and the answer is the layer that primarily owns the data. With
            // `write: "existing"` the upper layer is a sparse overlay whose directories
            // exist only as scaffolding to hold a shadowing file, and reporting their
            // creation time in place of the real directory's mtime would be misleading.
            if (up.isDir && write === "existing") {
                const low = await has(lower, ctx, path);
                if (low && low.isDir)
                    return low;
            }
            return up;
        },
        async readdir(ctx, path, opts) {
            let upList;
            let lowList;
            let lowErr;
            try {
                upList = await upper.readdir(ctx, path, opts);
            }
            catch (err) {
                const code = err.code;
                if (code !== "ENOENT" && code !== "ENOTDIR")
                    throw err;
            }
            try {
                lowList = await lower.readdir(ctx, path, opts);
            }
            catch (err) {
                const code = err.code;
                if (code !== "ENOENT" && code !== "ENOTDIR")
                    throw err;
                lowErr = err;
            }
            if (!upList && !lowList) {
                // Neither layer has it. Rethrow the error already in hand rather than
                // asking again — the lower layer is usually a network filesystem, and
                // re-running the listing to reproduce its ENOENT would double the cost of
                // every miss.
                throw lowErr;
            }
            // Merge by path, upper winning. Keyed on the full entry path rather than the
            // basename so a recursive listing dedupes correctly at every depth.
            const merged = new Map();
            for (const e of lowList?.entries ?? [])
                merged.set(e.path, e);
            for (const e of upList?.entries ?? [])
                merged.set(e.path, e);
            return {
                entries: [...merged.values()],
                // Only exhaustive if both halves were.
                complete: (upList?.complete ?? true) && (lowList?.complete ?? true),
            };
        },
        async readFile(ctx, path) {
            return (await readLayer(ctx, path)).readFile(ctx, path);
        },
        async readRange(ctx, path, offset, length) {
            const target = await readLayer(ctx, path);
            if (target.readRange)
                return target.readRange(ctx, path, offset, length);
            const whole = await target.readFile(ctx, path);
            return whole.subarray(offset, offset + length);
        },
        async writeFile(ctx, path, data) {
            const target = await writeLayer(ctx, path);
            if (target === upper && write === "upper") {
                // The upper layer may not have the containing directory yet — the whole
                // point of the archive case is that `node_modules/...` exists only in the
                // lower layer until something writes there.
                await upper.mkdir(ctx, dirname(path), { recursive: true });
            }
            await target.writeFile(ctx, path, data);
        },
        async mkdir(ctx, path, opts) {
            const target = write === "upper" ? upper : await writeLayer(ctx, path);
            return target.mkdir(ctx, path, opts);
        },
        async rm(ctx, path, opts) {
            if (await has(upper, ctx, path))
                return upper.rm(ctx, path, opts);
            // Present only below, where nothing can be removed and nothing can record that
            // it was.
            if (await has(lower, ctx, path)) {
                if (write === "upper") {
                    if (opts.force)
                        return;
                    roFail(ctx);
                }
                return lower.rm(ctx, path, opts);
            }
            if (opts.force)
                return;
            return upper.rm(ctx, path, opts); // for its ENOENT
        },
        async rename(ctx, from, to) {
            if (await has(upper, ctx, from))
                return upper.rename(ctx, from, to);
            if (write === "upper" && (await has(lower, ctx, from)))
                roFail(ctx);
            return lower.rename(ctx, from, to);
        },
        async utimes(ctx, path, atimeMs, mtimeMs) {
            if (await has(upper, ctx, path)) {
                return upper.utimes(ctx, path, atimeMs, mtimeMs);
            }
            if (write === "upper" && (await has(lower, ctx, path)))
                roFail(ctx);
            return lower.utimes(ctx, path, atimeMs, mtimeMs);
        },
        // statfs is intentionally absent: capacity belongs to whichever backend actually
        // stores bytes, and a union has no single answer.
    };
    // Attached conditionally, because the mount snapshot derives `canStream` from whether
    // this method exists and the worker uses that to pick a read strategy. Declaring it
    // unconditionally and throwing for a layer that cannot stream would make the
    // advertised capability a lie; declaring it never would stop a union mount from
    // streaming at all, which is the limitation that made `createReadStream` fall back to
    // per-chunk reads before this moved host-side.
    //
    // So: advertise iff *some* layer can stream, and synthesize for the case where the
    // layer actually chosen is the one that cannot.
    if (upper.openRead || lower.openRead) {
        provider.openRead = async (ctx, path, range) => {
            const target = await readLayer(ctx, path);
            if (target.openRead)
                return target.openRead(ctx, path, range);
            return streamOfBytes(await target.readFile(ctx, path), range);
        };
    }
    return provider;
}

// A filesystem over a `FileSystemDirectoryHandle`.
//
// One provider for both places those come from, because they are the same object:
//
//   navigator.storage.getDirectory()   → OPFS: private, persistent, no permission prompt
//   showDirectoryPicker()              → a real directory the user chose
//
// This is the backend the whole move existed to make possible. Every method here awaits
// something, and under the old worker-side contract that was simply not expressible: the same
// code had to satisfy `fs.readFileSync` over a blocking transport, so a provider could not
// await at all. Nothing here is unusual now — it is ordinary async web code.
//
// ## What this backend cannot do
//
//   - **Timestamps.** A `File` has `lastModified`, and that is the only time available: there is
//     no created or accessed time, and no way to *set* any of them. So `utimes` reports `false`
//     and directories report 0 rather than inventing a value that would jitter every stat and
//     defeat anything caching on mtime.
//   - **Rename.** `FileSystemHandle.move()` exists in Chromium and is the only atomic option;
//     elsewhere this degrades to copy-then-delete, which for a directory means a recursive walk.
//   - **Locking is real here.** An open `FileSystemWritableFileStream` excludes other writers to
//     the same file, including another tab. That surfaces as EBUSY rather than a hang: writes are
//     serialized per path within this provider so it never fights itself, and a lock held
//     elsewhere is reported instead of waited on.
function segments(path) {
    return path.split("/").filter((s) => s.length > 0 && s !== ".");
}
/**
 * Translate a DOMException into the errno a filesystem would report.
 *
 * The mapping is the whole reason a caller can treat this like any other backend: without it
 * every `catch (e) { if (e.code !== "ENOENT") throw e }` upstream — which is the dominant idiom
 * in this tree — would rethrow on a perfectly ordinary missing file.
 */
function translate(err, ctx) {
    const name = err?.name;
    // Always `reportPath`, never the mount-local path: a provider works in its own terms but an
    // error has to name the path the caller asked about. A mount at `/opfs` failing on its local
    // `/conf/x` must report `/opfs/conf/x`.
    const at = { ...ctx, path: ctx.reportPath };
    switch (name) {
        case "NotFoundError":
            throw fsError("ENOENT", at);
        case "TypeMismatchError":
            // Asked for a file and found a directory, or the reverse. Which errno depends on what
            // was asked for, and the caller knows: `syscall` carries it.
            throw fsError(at.syscall === "scandir" ? "ENOTDIR" : "EISDIR", at);
        case "NotAllowedError":
            // Permission was not granted, or was revoked.
            throw fsError("EACCES", at);
        case "SecurityError":
            throw fsError("EACCES", at);
        case "InvalidModificationError":
            // `removeEntry` on a non-empty directory without `recursive`.
            throw fsError("ENOTEMPTY", at);
        case "NoModificationAllowedError":
            // Someone else holds a writable on this file — another tab, or another handle.
            throw fsError("EBUSY", at);
        case "QuotaExceededError":
            throw fsError("ENOSPC", at);
        case "InvalidStateError":
            throw fsError("EIO", at);
        default:
            throw err;
    }
}
function createDirectoryHandleProvider(root, opts = {}) {
    const name = opts.name ?? "fs-handle";
    const prefix = opts.prefix ?? "";
    const events = opts.events ?? NO_EVENTS;
    /** The absolute path a local one corresponds to, for watch events. */
    function full(local) {
        if (!prefix)
            return local;
        return local === "/" ? prefix : prefix + local;
    }
    function assertWritable(ctx) {
        if (opts.readOnly)
            throw fsError("EROFS", ctx);
    }
    /**
     * Writes to one path are serialized, because a `FileSystemWritableFileStream` takes an
     * exclusive lock: two overlapping writes to the same file through this provider would make it
     * fail against itself with `NoModificationAllowedError`. A lock held *outside* this provider
     * is still reported as EBUSY — it is not ours to wait for.
     */
    const writeChains = new Map();
    function serialize(path, fn) {
        const prev = writeChains.get(path) ?? Promise.resolve();
        // `then(fn, fn)` rather than `then(fn)`: a failed write must not wedge the chain behind it.
        const next = prev.then(fn, fn);
        const settled = next.then(() => undefined, () => undefined);
        writeChains.set(path, settled);
        // Dropped once this is still the tail, so a long session does not retain one promise per
        // file it ever touched. If another write queued behind us the entry is theirs now.
        void settled.then(() => {
            if (writeChains.get(path) === settled)
                writeChains.delete(path);
        });
        return next;
    }
    async function dirAt(path, ctx, create = false) {
        let dir = root;
        for (const seg of segments(path)) {
            try {
                dir = await dir.getDirectoryHandle(seg, { create });
            }
            catch (err) {
                translate(err, ctx);
            }
        }
        return dir;
    }
    async function fileAt(path, ctx, create = false) {
        const parent = await dirAt(dirname(path), ctx);
        try {
            return await parent.getFileHandle(basename(path), { create });
        }
        catch (err) {
            translate(err, ctx);
        }
    }
    /** A file's `File`, which is where size and mtime come from. */
    async function fileOf(path, ctx) {
        const handle = await fileAt(path, ctx);
        try {
            return await handle.getFile();
        }
        catch (err) {
            translate(err, ctx);
        }
    }
    function entryOfFile(path, file) {
        return {
            path,
            name: basename(path) || "/",
            uid: "",
            isDir: false,
            isSymlink: false,
            size: file.size,
            // The only timestamp this backend has. Reported for all three rather than left at 0,
            // because `mtime` is the one anything actually reads.
            modifiedMs: file.lastModified,
            createdMs: file.lastModified,
            accessedMs: file.lastModified,
        };
    }
    function entryOfDir(path) {
        return {
            path,
            name: basename(path) || "/",
            uid: "",
            isDir: true,
            isSymlink: false,
            size: 0,
            // Deliberately 0, not `Date.now()`: a directory has no timestamp here, and inventing
            // one would change on every stat and defeat anything comparing mtimes.
            modifiedMs: 0,
            createdMs: 0,
            accessedMs: 0,
        };
    }
    /** Whether a path is a directory, a file, or absent — one probe, both answers. */
    async function kindOf(path, ctx) {
        if (segments(path).length === 0)
            return { kind: "dir" };
        const parent = await dirAt(dirname(path), ctx);
        const base = basename(path);
        try {
            const handle = await parent.getFileHandle(base);
            return { kind: "file", file: await handle.getFile() };
        }
        catch (err) {
            const name = err?.name;
            if (name !== "TypeMismatchError" && name !== "NotFoundError") {
                translate(err, ctx);
            }
            // Not a file. Either a directory or genuinely absent, and `getDirectoryHandle` says
            // which — its NotFoundError becomes the ENOENT.
            try {
                await parent.getDirectoryHandle(base);
                return { kind: "dir" };
            }
            catch (dirErr) {
                translate(dirErr, ctx);
            }
        }
    }
    /** The byte-writing half of `writeFile`, without the event — a copy is not a write. */
    async function writeBytes(path, data, ctx) {
        await serialize(path, async () => {
            // `create: true` so a write to a new path works, as every other backend here does.
            const handle = await fileAt(path, ctx, true);
            let writable;
            try {
                // Truncating: this is a whole-file write, and leaving existing data would turn a
                // shorter write into a partial overwrite.
                writable = await handle.createWritable({ keepExistingData: false });
            }
            catch (err) {
                translate(err, ctx);
            }
            try {
                // The cast is the SharedArrayBuffer case in the DOM types: a `Uint8Array` may in
                // principle be backed by one, which `write` does not accept. Nothing here ever
                // produces a shared buffer — frames are decoded into ordinary ones.
                await writable.write(data);
                await writable.close();
            }
            catch (err) {
                // Best-effort: a failed write must not leave the lock held.
                await writable.abort().catch(() => undefined);
                translate(err, ctx);
            }
        });
    }
    async function removeTree(path, ctx) {
        const parent = await dirAt(dirname(path), ctx);
        try {
            await parent.removeEntry(basename(path), { recursive: true });
        }
        catch (err) {
            translate(err, ctx);
        }
    }
    /**
     * Move a directory by copying it and deleting the original.
     *
     * Destination handling follows `rename(2)` rather than what is convenient: an existing empty
     * directory is replaced, a non-empty one is ENOTEMPTY, and a file is ENOTDIR. Merging into an
     * existing tree would be the easy thing to write here and is not a rename.
     */
    async function moveDirectory(from, to, ctx) {
        // One `move` event for the whole tree, emitted by the caller — not a write per copied file,
        // which would describe how the rename was implemented rather than what happened.
        let existing;
        try {
            existing = await kindOf(to, ctx);
        }
        catch {
            // Absent, which is the ordinary case.
        }
        if (existing?.kind === "file")
            throw fsError("ENOTDIR", ctx);
        if (existing?.kind === "dir") {
            const listing = await dirAt(to, ctx);
            for await (const _ of listing.keys()) {
                throw fsError("ENOTEMPTY", ctx);
            }
            const parent = await dirAt(dirname(to), ctx);
            try {
                await parent.removeEntry(basename(to));
            }
            catch (err) {
                translate(err, ctx);
            }
        }
        const copy = async (src, dst) => {
            await dirAt(dst, ctx, true);
            const dir = await dirAt(src, ctx);
            // Materialized before copying: adding entries to a directory while iterating it is not
            // something the api defines, and the destination can be inside the source's parent.
            const children = [];
            for await (const pair of dir.entries()) {
                children.push(pair);
            }
            for (const [name, handle] of children) {
                const childFrom = src === "/" ? `/${name}` : `${src}/${name}`;
                const childTo = dst === "/" ? `/${name}` : `${dst}/${name}`;
                if (handle.kind === "directory") {
                    await copy(childFrom, childTo);
                    continue;
                }
                const file = await handle.getFile();
                await writeBytes(childTo, new Uint8Array(await file.arrayBuffer()), ctx);
            }
        };
        await copy(from, to);
        await removeTree(from, ctx);
    }
    async function collect(dir, base, out, recursive, depth, maxEntries) {
        for await (const [childName, handle] of dir.entries()) {
            if (out.length >= maxEntries)
                return false;
            const childPath = base === "/" ? `/${childName}` : `${base}/${childName}`;
            if (handle.kind === "directory") {
                out.push(entryOfDir(childPath));
                if (recursive && depth > 1) {
                    const complete = await collect(handle, childPath, out, recursive, depth - 1, maxEntries);
                    if (!complete)
                        return false;
                }
            }
            else {
                out.push(entryOfFile(childPath, await handle.getFile()));
            }
        }
        return true;
    }
    return {
        name,
        async stat(ctx, path) {
            const found = await kindOf(path, ctx);
            return found.kind === "file"
                ? entryOfFile(path, found.file)
                : entryOfDir(path);
        },
        async readdir(ctx, path, o) {
            const found = await kindOf(path, ctx);
            if (found.kind !== "dir")
                throw fsError("ENOTDIR", ctx);
            const dir = await dirAt(path, ctx);
            const out = [];
            const maxEntries = o?.maxEntries ?? Infinity;
            const complete = await collect(dir, segments(path).length === 0 ? "/" : path, out, !!o?.recursive, o?.depth ?? Infinity, maxEntries);
            return { entries: out, complete };
        },
        async readFile(ctx, path) {
            const file = await fileOf(path, ctx);
            return new Uint8Array(await file.arrayBuffer());
        },
        /**
         * A genuine positioned read: `Blob.slice` does not move the earlier bytes.
         *
         * Worth being sure about, because the mount snapshot advertises this and a caller keeps a
         * byte-range cache on the strength of it. Claiming a native range that actually re-reads
         * the file would make a positioned-read loop quadratic.
         */
        async readRange(ctx, path, offset, length) {
            const file = await fileOf(path, ctx);
            const slice = file.slice(offset, offset + length);
            return new Uint8Array(await slice.arrayBuffer());
        },
        async openRead(ctx, path, range) {
            const file = await fileOf(path, ctx);
            const start = range?.start ?? 0;
            // node's `end` is inclusive.
            const end = range?.end === undefined ? file.size : range.end + 1;
            const blob = start === 0 && end >= file.size ? file : file.slice(start, end);
            return { size: blob.size, stream: blob.stream() };
        },
        async writeFile(ctx, path, data) {
            assertWritable(ctx);
            await writeBytes(path, data, ctx);
            events.write(full(path));
        },
        async mkdir(ctx, path, o) {
            assertWritable(ctx);
            const parts = segments(path);
            if (parts.length === 0)
                throw fsError("EEXIST", ctx);
            if (!o.recursive) {
                // The parent must exist and the target must not — neither of which
                // `getDirectoryHandle({create:true})` checks, since it is idempotent and creates
                // only the last segment.
                const parent = await dirAt(dirname(path), ctx);
                const base = basename(path);
                let exists = true;
                try {
                    await parent.getDirectoryHandle(base);
                }
                catch (err) {
                    const errName = err?.name;
                    if (errName === "TypeMismatchError")
                        throw fsError("EEXIST", ctx);
                    if (errName !== "NotFoundError")
                        translate(err, ctx);
                    exists = false;
                }
                if (exists)
                    throw fsError("EEXIST", ctx);
                try {
                    await parent.getDirectoryHandle(base, { create: true });
                }
                catch (err) {
                    translate(err, ctx);
                }
                events.add(full(path), true);
                return undefined;
            }
            await dirAt(path, ctx, true);
            events.add(full(path), true);
            // node reports the first directory a recursive mkdir created. Learning that would mean
            // probing every ancestor first, so it is left undefined — the same answer puterfs gives.
            return undefined;
        },
        async rm(ctx, path, o) {
            assertWritable(ctx);
            const parts = segments(path);
            if (parts.length === 0)
                throw fsError("EPERM", ctx);
            let isDir = false;
            try {
                isDir = (await kindOf(path, ctx)).kind === "dir";
            }
            catch (err) {
                if (o.force && err.code === "ENOENT")
                    return;
                throw err;
            }
            const parent = await dirAt(dirname(path), ctx);
            try {
                await parent.removeEntry(basename(path), { recursive: o.recursive });
            }
            catch (err) {
                if (o.force && err?.name === "NotFoundError")
                    return;
                translate(err, ctx);
            }
            events.remove(full(path), isDir);
        },
        async rename(ctx, from, to) {
            assertWritable(ctx);
            const found = await kindOf(from, ctx);
            // `move` is atomic and is the only correct answer, but it is Chromium-only (and for a
            // long time OPFS-only). Probed rather than assumed, so this works either way.
            const handle = found.kind === "file"
                ? await fileAt(from, ctx)
                : await dirAt(from, ctx);
            const movable = handle;
            if (typeof movable.move === "function") {
                const destParent = await dirAt(dirname(to), ctx);
                try {
                    await movable.move(destParent, basename(to));
                    events.move(full(from), full(to), found.kind === "dir");
                    return;
                }
                catch (err) {
                    const errName = err?.name;
                    // Not supported for this handle after all; fall through to the copy.
                    if (errName !== "NotSupportedError" && errName !== "TypeError") {
                        translate(err, ctx);
                    }
                }
            }
            if (found.kind === "dir") {
                // A directory move without `move()` is a recursive copy plus a delete.
                //
                // This used to report EXDEV instead, on the reasoning that a caller renaming a
                // directory into place is relying on atomicity and half-doing it is worse than
                // declining. That was wrong about what declining costs: `vite dev` renames
                // `node_modules/.vite/deps_temp_*` over `deps` on every startup and does not catch
                // the failure, so EXDEV here means the dev server does not run at all. A
                // non-atomic move is a real limitation; refusing to move is not a smaller one.
                //
                // Where `move()` exists — Chromium's OPFS — none of this runs and the rename is
                // atomic. The gap is documented on `createDirectoryHandleProvider`.
                await moveDirectory(from, to, ctx);
                events.move(full(from), full(to), true);
                return;
            }
            const bytes = new Uint8Array(await found.file.arrayBuffer());
            await this.writeFile({ ...ctx, reportPath: to }, to, bytes);
            await this.rm(ctx, from, { recursive: false, force: false });
            events.move(full(from), full(to), false);
        },
        /**
         * Not representable: there is no api to set a timestamp on a `FileSystemFileHandle`.
         *
         * `false` is a normal answer rather than an error, and the composed `utimes` above still
         * validates the path — so `utimes` on a missing file reports ENOENT as node requires.
         */
        async utimes() {
            return false;
        },
        async statfs() {
            // Origin-wide, not per-directory — which is what `statfs(2)` reports too. Only
            // meaningful for OPFS; a picked directory has no quota to report and answers zeroes.
            const estimate = await navigator.storage?.estimate?.();
            return {
                used: estimate?.usage ?? 0,
                capacity: estimate?.quota ?? 0,
            };
        },
    };
}
/**
 * Whether this handle can be used, asking for permission if it has not been granted.
 *
 * OPFS needs none of this — it is same-origin private storage. A handle from
 * `showDirectoryPicker()` does, its grant does not survive a reload, and re-requesting requires a
 * user gesture. So this must be called from a click handler, not from mount time, and mounting a
 * handle without a grant would otherwise fail per-operation with EACCES instead of once, up front.
 */
async function ensureDirectoryHandleAccess(handle, mode = "readwrite") {
    const withPermissions = handle;
    if (!withPermissions.queryPermission)
        return true; // OPFS, or an engine without the api
    if ((await withPermissions.queryPermission({ mode })) === "granted")
        return true;
    return (await withPermissions.requestPermission?.({ mode })) === "granted";
}

// The host-side filesystem a `NodeWorker` runs on.
//
// One of these per worker by default. Every worker has had its own `/tmp`, its own overlay
// and its own memory mounts for as long as those have existed, and a single shared namespace
// would silently start sharing all three — wrong for injected modules, and wrong for a
// consumer that populates a project per worker and treats each as a private replica. Pass an
// explicit instance to opt into sharing.
//
// What used to be eight page↔worker message types (`mem-mount`, `mem-write`, `mem-read`,
// `mem-list`, `mem-remove`, `mem-unmount`, `vmodule-add`, `vmodule-remove`) is now ordinary
// method calls on this object, and **synchronous**: there is no boundary left to cross. That
// also retires `writeMemory`'s `{transfer: true}` hazard, where populating a mount detached
// every buffer you passed.
const encoder = new TextEncoder();
class NodeVfs {
    sid;
    #table = new MountTable();
    #facade;
    #handles;
    #api;
    #events;
    /** The sparse in-memory layer over the root mount. Also where injected files land. */
    #overlay;
    /** Per-mount injection overlays, from `mount(..., { overlay: true })`. */
    #overlays = new Map();
    #tmp;
    /** Memory mounts the host created, by root. */
    #memoryMounts = new Map();
    /** Queued for the reply of the call that caused them. */
    #replies = createReplayCache();
    /**
     * How deep inside `handleFrame` we are, which is what tells a worker-caused mutation from a
     * host-caused one. A counter rather than a flag because two frames can be in flight at once —
     * the asynchronous transport does not wait for one call before accepting the next.
     */
    #dispatchDepth = 0;
    #pendingEvents = [];
    #pendingPaths = new Set();
    #pendingSubtrees = new Set();
    #listeners = new Set();
    #mountListeners = new Set();
    /** The read cache in front of puterfs, when there is one. */
    #cache;
    #feed;
    /**
     * Events this filesystem produced itself, so the cache can ignore them coming
     * back around.
     *
     * Every local mutation is fanned out to the change feed by `NodeWorker`, and
     * the feed hands it straight back to the subscription below. Re-applying it
     * would be worse than wasteful: a mutation is invalidated *precisely* by the
     * cache that performed it — an overwrite keeps the enclosing listings, and the
     * bytes just written are kept — whereas the generic event handler can only
     * assume the worst and drop both.
     *
     * Identity, not a copy, because in-page delivery hands over the same object.
     * A `WeakSet` so an event nobody echoes back is not a leak. This is
     * deliberately per-filesystem: another `NodeVfs` sharing the feed fronts the
     * same puterfs and *does* need to hear about this one's writes.
     */
    #ownEvents = new WeakSet();
    constructor(opts = {}) {
        this.sid = opts.sid ?? randomSid();
        this.#facade = new Facade(this.#table);
        this.#handles = new HandleRegistry(this.#facade);
        this.#events = createFsEvents((event) => this.#emit(event));
        this.#table.onChange(() => {
            const snapshot = this.#table.snapshot();
            for (const fn of [...this.#mountListeners])
                fn(snapshot);
        });
        // Mounted at "/", so a local path already is the absolute one.
        this.#overlay = createMemoryProvider({
            name: "overlay",
            prefix: "",
            events: this.#events,
        });
        if (opts.puter) {
            this.#api = new PuterApi(opts.puter.token, opts.puter.apiOrigin);
            const puter = createPuterProvider({
                api: this.#api,
                events: this.#events,
            });
            // The cache goes *under* the union rather than over it. The overlay above
            // is memory, so its probes cost nothing and there is nothing to cache
            // about them — everything worth caching is exactly what reaches puterfs,
            // which is what this position sees.
            //
            // Subscribing only when there is a cache to feed: the subscription is what
            // holds the socket open, and opening one to invalidate a cache that does
            // not exist would be a connection nothing reads.
            let root = puter;
            if (opts.puter.cache?.enabled !== false) {
                const feed = subscribeFsEvents(opts.puter.token, this.#api.origin, (msg) => this.#onFeed(msg));
                this.#feed = feed;
                this.#cache = createCachingProvider(puter, {
                    // Every listing here is a network round trip, which is the whole case for
                    // the seeding in ./cache.ts. Before the spread, so a caller can say no.
                    prefetch: true,
                    ...opts.puter.cache,
                    // Mounted at "/", so a provider-local path already is the absolute one
                    // the feed reports.
                    prefix: "/",
                    freshness: feed,
                });
                root = this.#cache;
            }
            // `"existing"` rather than `"upper"`: a write to a path the overlay holds updates
            // the overlay, but a write to an ordinary path still goes to ordinary storage.
            // Routing every write into memory would silently stop persisting anything.
            this.#table.mount("/", unionProvider(this.#overlay, root, "existing"));
        }
        else {
            this.#table.mount("/", this.#overlay);
        }
        if (opts.tmp !== false) {
            // `os.tmpdir()` has always returned "/tmp", but puterfs has no such directory and
            // cannot grow one: the root holds user home directories and writing to it is
            // refused, so `mkdir("/tmp")` fails and every `mkdtemp`-style caller was pointed
            // at an unusable path. An in-memory mount is what that path should have been —
            // scratch space that is fast, private, and gone when the worker is.
            this.#tmp = createMemoryProvider({
                name: "tmp",
                prefix: "/tmp",
                events: this.#events,
            });
            this.#table.mount("/tmp", this.#tmp);
        }
        if (opts.dev !== false) {
            // `os.devNull` has always answered "/dev/null", and until now nothing provided it — so
            // every `>/dev/null` in a shell script failed at the redirect. See ./dev.ts.
            this.#table.mount("/dev", createDevProvider({ events: this.#events }));
        }
    }
    // -------------------------------------------------------------- the mount table
    /**
     * Mount a provider at `root`.
     *
     * Takes a **factory** as well as a provider, and that is not sugar: a provider needs the
     * session's watch-event sink and its own mount prefix in order to report `fs.watch` events,
     * and a caller who constructed it themselves has neither. Passing a factory lets this supply
     * both, correctly — where handing out the sink and asking the caller to also pass a matching
     * prefix would silently produce events with the wrong paths whenever the two disagreed.
     *
     *   vfs.mount("/opfs", (m) => createDirectoryHandleProvider(handle, m));
     *
     * `overlay` puts a sparse in-memory layer in front of the provider — the same arrangement `/`
     * has over puterfs. It exists for `addVirtualFile`/`registerVirtualModule`, which need
     * *somewhere* to put a module that has no business being written to the backend: a run's entry
     * point has to sit inside the project to resolve the project's `node_modules`, and persisting
     * it there would leave litter behind on every run. Writes to paths the overlay does not hold
     * still go to the provider, so an ordinary file write is unaffected.
     *
     * `cache` puts the read cache (./cache.ts) in front of the provider. **On by default for a
     * read-only mount**, off otherwise, and that default is the whole of the reasoning: a mount
     * nothing can write through cannot go stale by anything this filesystem does, and there is no
     * change feed for a `FileSystemDirectoryHandle` or a zip to tell us about anyone else. A
     * writable mount gets nothing by default, because "nobody else touches it" is a claim only the
     * consumer can make — pass `{}` to make it.
     */
    mount(root, provider, opts) {
        const normalized = normalizeRoot(root);
        const built = typeof provider === "function"
            ? provider({ events: this.#events, prefix: normalized })
            : provider;
        let mounted = this.#cached(built, normalized, opts);
        if (opts?.overlay) {
            const overlay = createMemoryProvider({
                name: `overlay:${normalized}`,
                prefix: normalized,
                events: this.#events,
            });
            this.#overlays.set(normalized, overlay);
            // "existing" and not "upper": a write goes to whichever layer already holds the path,
            // so only what was injected here stays here and everything else reaches the backend.
            // Over `mounted`, not `built`: the cache belongs under the overlay, where the
            // backend is, for the same reason it does at "/".
            mounted = unionProvider(overlay, mounted, "existing");
        }
        this.#table.mount(normalized, mounted, opts);
        // Anything the worker's resolver concluded about this subtree — including "there is
        // nothing here", which it derives from a fully-listed ancestor — predates the mount
        // and is now wrong.
        this.#pendingSubtrees.add(normalized);
    }
    /**
     * The read cache for a mount other than "/", when it should have one.
     *
     * Deliberately not tracked in `#cache`, which is the puterfs one: that cache has a
     * change feed behind it and these have none, so nothing outside can invalidate them
     * and a stale mark would mean nothing if it arrived.
     */
    #cached(provider, prefix, opts) {
        const wanted = opts?.cache ?? (opts?.readOnly ? {} : undefined);
        if (!wanted || wanted.enabled === false)
            return provider;
        return createCachingProvider(provider, { ...wanted, prefix });
    }
    unmount(root) {
        const normalized = normalizeRoot(root);
        const removed = this.#table.unmount(normalized);
        if (removed)
            this.#pendingSubtrees.add(normalized);
        this.#memoryMounts.delete(normalized);
        this.#overlays.delete(normalized);
        return removed;
    }
    listMounts() {
        return this.#table.snapshot();
    }
    snapshot() {
        return this.#table.snapshot();
    }
    onMountsChanged(fn) {
        this.#mountListeners.add(fn);
        return () => this.#mountListeners.delete(fn);
    }
    // ------------------------------------------------------------- memory mounts
    /**
     * Create a memory-backed directory at `root`.
     *
     * `replace` swaps out an existing mount at the same root instead of throwing, which is
     * what re-populating a project between runs wants.
     */
    mountMemory(root, opts = {}) {
        const normalized = normalizeRoot(root);
        if (normalized === "/") {
            throw new Error("/ is already mounted; write to it directly instead");
        }
        if (this.#memoryMounts.has(normalized)) {
            if (!opts.replace)
                throw new Error(`already mounted: ${normalized}`);
            this.unmountMemory(normalized);
        }
        const provider = createMemoryProvider({
            name: `host:${normalized}`,
            prefix: normalized,
            events: this.#events,
        });
        this.#table.mount(normalized, provider, { readOnly: opts.readOnly });
        this.#memoryMounts.set(normalized, provider);
        this.#pendingSubtrees.add(normalized);
        return this.#wrap(normalized, provider);
    }
    unmountMemory(root) {
        const normalized = normalizeRoot(root);
        if (!this.#memoryMounts.delete(normalized))
            return;
        this.#table.unmount(normalized);
        this.#pendingSubtrees.add(normalized);
    }
    /** "/" is the sparse overlay over the root mount, rather than a mount of its own. */
    memory(root = "/") {
        const normalized = normalizeRoot(root);
        if (normalized === "/")
            return this.#wrap("/", this.#overlay);
        const provider = this.#memoryMounts.get(normalized);
        if (!provider)
            throw new Error(`no memory mount at ${normalized}`);
        return this.#wrap(normalized, provider);
    }
    #wrap(base, provider) {
        const abs = (local) => (base === "/" ? local : base + local);
        return {
            root: base,
            write: (entries) => {
                let bytes = 0;
                for (const entry of entries) {
                    // Entry paths are relative to the mount root. Resolving against "/" rather
                    // than against `base` keeps a `..` in a hostile or careless path from
                    // climbing out of the mount — it can only ever bottom out at the root.
                    const local = resolveFrom("/", entry.path);
                    const existed = provider.get(local) !== undefined;
                    if (entry.data === undefined) {
                        provider.mkdirp(local);
                        if (!existed)
                            this.#events.add(abs(local), true);
                        continue;
                    }
                    const data = toBytes(entry.data);
                    const file = provider.put(local, data);
                    if (entry.mtimeMs !== undefined)
                        file.mtimeMs = entry.mtimeMs;
                    bytes += data.length;
                    // Watch events, because these writes go straight to the provider and so
                    // bypass the facade that would otherwise emit them. Without this a host
                    // edit is invisible to `fs.watch` inside the runtime — which is to say a
                    // dev server never notices the file changed, and HMR never fires.
                    if (existed)
                        this.#events.write(abs(local));
                    else
                        this.#events.add(abs(local));
                    this.#pendingPaths.add(abs(local));
                }
                return { written: entries.length, bytes };
            },
            read: (path) => {
                const live = provider.get(resolveFrom("/", path));
                if (!live)
                    return undefined;
                // A copy: the caller may keep or mutate it, and the tree owns its bytes.
                return new Uint8Array(live);
            },
            list: (path, opts) => provider.list(resolveFrom("/", path), opts),
            remove: (paths) => {
                for (const path of paths) {
                    const local = resolveFrom("/", path);
                    // Directory-ness has to be read before the drop, and only matters to the
                    // watcher.
                    const wasDir = provider.list(local) !== undefined;
                    if (provider.drop(local))
                        this.#events.remove(abs(local), wasDir);
                    this.#pendingPaths.add(abs(local));
                }
            },
            mkdir: (path) => {
                const local = resolveFrom("/", path);
                provider.mkdirp(local);
                this.#events.add(abs(local), true);
                this.#pendingPaths.add(abs(local));
            },
        };
    }
    // ---------------------------------------------------------- injected modules
    /**
     * The in-memory layer that actually serves `path`, and the path within it.
     *
     * Not always the root overlay. Once a mount exists at, say, `/proj`, longest prefix wins and
     * every read under it resolves to *that* mount — so putting the file in the root overlay would
     * leave it permanently unreachable, shadowed by the very mount it appears to live in. So the
     * layer has to belong to the owning mount: either a memory mount, or the overlay a mount was
     * given by `mount(..., { overlay: true })`.
     */
    #injectionTarget(path) {
        const { mount: owner, local } = this.#table.resolve(path);
        if (owner.root === "/")
            return { provider: this.#overlay, local: path };
        const provider = this.#memoryMounts.get(owner.root) ?? this.#overlays.get(owner.root);
        if (!provider) {
            // Some other kind of backend owns this subtree — an OPFS directory, say. There is no
            // in-memory layer to put the file in, and silently writing it somewhere unreachable
            // would be worse than saying so. The fix is named because the error is otherwise a
            // dead end: nothing about "non-memory mount" suggests that a mount can be given a
            // memory layer without becoming one.
            throw new Error(`cannot inject at ${path}: ${owner.root} is served by a non-memory mount. ` +
                `Mount it with { overlay: true } to give it an in-memory layer for ` +
                `injected files.`);
        }
        return { provider, local };
    }
    addVirtualFile(path, code) {
        const resolved = resolveFrom("/", path);
        const { provider, local } = this.#injectionTarget(resolved);
        provider.put(local, toBytes(code));
        // The worker's resolver caches source text and stat results permanently. Injected
        // files are the one thing that can be replaced at a stable path, so the caches have
        // to be told — this is the invalidation that used to be a direct function call from
        // the worker's own `virtual.ts`, and dropping it would mean a second run compiling
        // the first run's source.
        this.#pendingPaths.add(resolved);
    }
    removeVirtualFile(path) {
        const resolved = resolveFrom("/", path);
        const { provider, local } = this.#injectionTarget(resolved);
        provider.drop(local);
        this.#pendingPaths.add(resolved);
    }
    // --------------------------------------------------- the filesystem, from here
    //
    // The host's own way in, and it is not a convenience: a write made *behind* the filesystem —
    // straight to OPFS, say — is invisible to it, so no watch event is emitted and anything
    // watching inside the runtime never learns the file changed. That is exactly how an editor
    // save stops triggering HMR. Going through the facade means a host write is an ordinary
    // filesystem write: the provider runs, the event fires, and watchers see it.
    //
    // Async, because that is what a provider is. The synchronous `memory()` API stays for memory
    // mounts, where there is nothing to await.
    async stat(path) {
        const p = resolveFrom("/", path);
        return this.#facade.stat({ syscall: "stat", reportPath: p }, p);
    }
    async readFile(path) {
        const p = resolveFrom("/", path);
        return this.#facade.readFile({ syscall: "open", reportPath: p }, p);
    }
    async writeFile(path, data) {
        const p = resolveFrom("/", path);
        await this.#facade.writeFile({ syscall: "write", reportPath: p }, p, toBytes(data));
    }
    async readdir(path, opts) {
        const p = resolveFrom("/", path);
        return this.#facade.readdir({ syscall: "scandir", reportPath: p }, p, opts);
    }
    async mkdir(path, opts = {}) {
        const p = resolveFrom("/", path);
        await this.#facade.mkdir({ syscall: "mkdir", reportPath: p }, p, {
            recursive: !!opts.recursive,
        });
    }
    async rm(path, opts = {}) {
        const p = resolveFrom("/", path);
        await this.#facade.rm({ syscall: "unlink", reportPath: p }, p, {
            recursive: !!opts.recursive,
            force: !!opts.force,
        });
    }
    // ------------------------------------------------------------------ events
    /**
     * Mutations this filesystem performs, with the session that caused them.
     *
     * Fires for every mutation this filesystem performs, whoever caused it — which is what a
     * consumer keeping a read model in step with the runtime needs.
     *
     * `causedBy` is the session id when a worker call caused it, and `undefined` when the host did.
     * The distinction matters for one thing only: a worker-caused mutation has already reached that
     * worker on the reply frame, so anything forwarding events *into* workers is choosing between
     * a duplicate and a drop. See `#emit` for why this one picks the duplicate.
     */
    onFsEvent(fn) {
        this.#listeners.add(fn);
        return () => this.#listeners.delete(fn);
    }
    /**
     * Two deliveries, deliberately overlapping.
     *
     * A mutation made **during a worker call** rides that call's reply. That is the only delivery
     * that reaches a worker parked inside a blocking request, and the only one that works for a
     * consumer with no token and therefore no events channel at all.
     *
     * Every mutation is *also* announced to listeners. That is the path a mutation made **outside**
     * a call has to take — a host write, an editor save — because there is no reply for it to ride
     * and the worker may be sitting in a watcher making no calls whatsoever. Missing this is
     * silent and oddly specific: everything keeps working except that saving a file stops
     * triggering HMR.
     *
     * So a worker can see one of its own writes twice, once per path. That is the cheap side of the
     * trade — watch events are deliberately not deduplicated anyway (`worker/fsevents.ts`), and a
     * repeated event costs a redundant rebuild where a dropped one costs a dev server that has
     * quietly stopped noticing edits. `causedBy` is there for a listener that would rather filter.
     */
    #emit(event) {
        const causedBy = this.#dispatchDepth > 0 ? this.sid : undefined;
        if (causedBy !== undefined)
            this.#pendingEvents.push(event);
        // Before the listeners, because one of them fans this out to the change
        // feed, which hands it straight back to `#onFeed` — synchronously. See
        // `#ownEvents`.
        this.#ownEvents.add(event);
        for (const fn of [...this.#listeners]) {
            try {
                fn(event, causedBy);
            }
            catch (err) {
                console.warn("[node-worker] fs event listener threw", err);
            }
        }
    }
    /**
     * The change feed, from the cache's point of view.
     *
     * `event` is the precise signal and `stale` the coarse one; the difference is
     * whether the source could name a path. See ./cache.ts on why a stale mark is
     * not a flush. `state` needs no handling — every transition of it is already
     * accompanied by a `stale`, since both edges leave an unobserved window.
     */
    #onFeed(msg) {
        if (!this.#cache)
            return;
        if (msg.op === "ev.fs") {
            if (this.#ownEvents.has(msg.event))
                return;
            this.#cache.applyEvent(msg.event);
            return;
        }
        if (msg.op === "ev.stale")
            this.#cache.markStale();
    }
    // -------------------------------------------------------------------- stats
    /** Per-endpoint backend call counts, the host half of `NODE_WORKER_API_STATS`. */
    apiStats() {
        return this.#api?.stats() ?? {};
    }
    /** Filesystem operations per mount — the count that only this side can see. */
    opStats() {
        return this.#facade.opStats();
    }
    /**
     * Hits, misses and what the read cache is holding.
     *
     * The number that explains the other two: `opStats` counts what the worker
     * asked for and `apiStats` counts what left the browser, and this is where the
     * difference went.
     */
    cacheStats() {
        return this.#cache?.stats() ?? {};
    }
    /** Drop everything cached about puterfs. Diagnostics, and a way out of a bad state. */
    flushCache() {
        this.#cache?.flush();
    }
    resetStats() {
        this.#api?.resetStats();
        this.#facade.resetOpStats();
    }
    // ------------------------------------------------------------- the transport
    /**
     * @internal — the one entry point both transports call.
     *
     * `sid` is the *transport* session the frame arrived on, which is not the same thing as this
     * filesystem's identity once more than one worker is mounted on it. The probe echoes it back so
     * a misrouted frame is still caught (see dispatch), and each worker checks the answer against
     * the id it was given. Omitted, it falls back to this vfs's own id — the single-worker case,
     * and what every caller did before this was a parameter.
     */
    async handleFrame(frame, sid = this.sid) {
        this.#dispatchDepth++;
        try {
            return await handleFrame(this.#deps(sid), frame);
        }
        finally {
            this.#dispatchDepth--;
        }
    }
    /**
     * Drop this session's open files.
     *
     * Called when the worker goes away, and it has to be: a handle lives here now and outlives
     * the worker that opened it, so without this every run leaks its open files — and for a
     * memory mount that means leaking the contents of unlinked files, which stay alive on
     * purpose for exactly as long as a handle refers to them.
     *
     * Dirty buffers are **not** flushed. A worker that died did not ask for its pending writes
     * to land, and inventing a flush would publish half-written files nobody asked to publish.
     */
    closeSession(sid) {
        // Scoped to the session, because one vfs may back several workers at once and the others
        // are still reading their descriptors. Without a sid this means "all of them", which is
        // what `dispose` wants and nothing else does.
        this.#handles.closeAll(sid);
        // The retry window for a worker that is gone can never be consulted again.
        if (sid !== undefined)
            forgetReplays(this.#replies, sid);
    }
    /**
     * Give up this filesystem for good.
     *
     * Distinct from `closeSession`, which ends one *worker's* use of a namespace
     * that may outlive it. This ends the namespace: the change feed is detached,
     * and with it the socket and the poll timer that only existed to keep the
     * cache honest.
     *
     * A `NodeWorker` calls this on the filesystem it created for itself. One
     * handed in from outside belongs to whoever handed it in — and a consumer that
     * builds a fresh `NodeVfs` per run has to call this, or every restart leaves a
     * socket behind.
     */
    dispose() {
        this.closeSession();
        this.#feed?.close();
        this.#feed = undefined;
        this.#cache?.flush();
    }
    /** How many fds this session currently holds. Diagnostics. */
    get openHandles() {
        return this.#handles.size;
    }
    /** @internal — `createReadStream`, which is async-only and so sits outside the frames. */
    openRead(path, range) {
        return this.#facade.openRead({ syscall: "read", reportPath: path }, resolveFrom("/", path), range);
    }
    /**
     * @internal — a stream over an open fd, for `createReadStream({ fd })`.
     *
     * Separate from `openRead` because a handle may hold bytes the backend has not seen: those
     * are the file as far as that fd is concerned, and streaming the backend's version instead
     * would silently serve stale content.
     */
    openReadFd(fd, range) {
        return this.#handles.get(fd, "read").openRead(range);
    }
    #deps(sid = this.sid) {
        return {
            fs: this.#facade,
            table: this.#table,
            handles: this.#handles,
            replies: this.#replies,
            sid,
            proto: WIRE_PROTO,
            openRead: (path, range) => this.openRead(path, range),
            // Supplied at last. `drainApiCalls` was optional in `DispatchDeps`, consumed by the
            // worker's `applyMeta`, and handed in by nobody — so `NODE_WORKER_API_STATS` could
            // only ever report the worker's own hop counts and never what actually left the
            // browser. Drained per reply, so the numbers are attributable to the call that
            // caused them.
            drainApiCalls: () => this.#api?.drainStats?.(),
            openReadFd: (fd, range) => this.openReadFd(fd, range),
            drainEvents: () => {
                const out = this.#pendingEvents;
                this.#pendingEvents = [];
                return out;
            },
            drainInvalidations: () => {
                if (this.#pendingPaths.size === 0 && this.#pendingSubtrees.size === 0) {
                    return undefined;
                }
                const out = {
                    paths: this.#pendingPaths.size ? [...this.#pendingPaths] : undefined,
                    subtrees: this.#pendingSubtrees.size
                        ? [...this.#pendingSubtrees]
                        : undefined,
                };
                this.#pendingPaths.clear();
                this.#pendingSubtrees.clear();
                return out;
            },
        };
    }
}
function randomSid() {
    return [...Array(10)].reduce((a) => a + Math.random().toString(36)[2], "");
}
function toBytes(data) {
    if (typeof data === "string")
        return encoder.encode(data);
    if (data instanceof ArrayBuffer)
        return new Uint8Array(data);
    // A copy, because the tree must own its bytes rather than pin whatever the caller
    // happened to slice this view out of.
    return new Uint8Array(data);
}
/** Absolute, no trailing slash, no `.`/`..`/`//` — the form the mount table demands. */
function normalizeRoot(root) {
    const resolved = resolveFrom("/", root);
    return resolved.length > 1 && resolved.endsWith("/")
        ? resolved.slice(0, -1)
        : resolved;
}

// Registering the service worker and attaching a session to it.
//
// The page is the filesystem host: the service worker relays a blocking request from the node
// worker to here, `handleFrame` answers it, and the answer becomes the XHR's response body.
//
// Everything in this file exists to make one failure mode impossible: a synchronous `fs` call
// that hangs because interception silently is not happening. That is what the scope assertion
// and the startup probe are for — a misconfiguration should be a startup error naming the fix,
// never a worker parked forever.
/** How long one attach handshake is given, and how many are tried. */
const ATTACH_TIMEOUT_MS = 5_000;
const ATTACH_ATTEMPTS = 3;
/** One registration per (url, scope), shared by every session on the page. */
const registrations = new Map();
/**
 * Does this registration still own its scope?
 *
 * A registration is not forever. Another app on the same origin can claim the scope, and
 * clearing site data takes it away outright — and neither is visible from the object, which
 * goes on reporting an `active` worker that still answers `postMessage`. So the handshake
 * succeeds and every synchronous request 404s instead, which reads as a filesystem fault
 * rather than as the registration being gone.
 */
async function ownsScope(reg) {
    try {
        const current = await navigator.serviceWorker.getRegistration(reg.scope);
        return current === reg && !!reg.active;
    }
    catch {
        // Cannot tell — assume it is fine rather than re-registering on every session.
        return true;
    }
}
async function registerOnce(swURL, scope) {
    // See the same key in ./fsevents.ts on why NUL, and why as an escape rather than the
    // literal byte: an embedded NUL makes grep treat the file as binary.
    const key = `${swURL}\0${scope ?? ""}`;
    let existing = registrations.get(key);
    if (existing) {
        const cached = await existing.catch(() => undefined);
        if (cached && (await ownsScope(cached)))
            return cached;
        // Only if nobody has replaced it meanwhile, so concurrent sessions do not each
        // register a fresh one.
        if (registrations.get(key) === existing)
            registrations.delete(key);
        existing = undefined;
    }
    if (!existing) {
        existing = navigator.serviceWorker
            .register(swURL, scope ? { scope } : undefined)
            .then(async (reg) => {
            await activeOf(reg);
            return reg;
        });
        // A failed registration must not be cached as the answer forever.
        existing.catch(() => registrations.delete(key));
        registrations.set(key, existing);
    }
    return existing;
}
/**
 * What the registration looks like right now, for an error that has to be diagnosed remotely.
 *
 * A bare "did not acknowledge" names a symptom shared by every cause — a worker that was
 * stopped at the wrong moment, one replaced mid-handshake, a scope taken over. These four
 * states tell those apart in a report from someone else's browser.
 */
function describeWorkerState(reg) {
    const state = (w) => w?.state ?? "none";
    return (`scope=${new URL(reg.scope).pathname} active=${state(reg.active)} ` +
        `waiting=${state(reg.waiting)} installing=${state(reg.installing)} ` +
        `controller=${state(navigator.serviceWorker.controller)}`);
}
/** One handshake went unanswered. Retryable, unlike everything else `connect` can throw. */
class AttachTimeout extends Error {
    constructor() {
        super("service worker did not acknowledge the session");
        this.name = "AttachTimeout";
    }
}
/**
 * Wait for an active worker.
 *
 * Deliberately not `navigator.serviceWorker.ready`, which resolves only for a registration
 * whose scope contains *this page* — and a scope that covers the worker script but not the page
 * is a perfectly valid arrangement (measured: it intercepts). Awaiting `ready` there would hang
 * forever.
 */
function activeOf(reg) {
    if (reg.active)
        return Promise.resolve(reg.active);
    return new Promise((resolve, reject) => {
        const deadline = setTimeout(() => reject(new Error("service worker never became active")), 10_000);
        const check = () => {
            if (reg.active) {
                clearTimeout(deadline);
                resolve(reg.active);
                return;
            }
            setTimeout(check, 50);
        };
        check();
    });
}
/**
 * Why interception cannot work here, if it cannot.
 *
 * Returns `undefined` when nothing is obviously wrong — which is not the same as "it works",
 * hence the probe the worker runs afterwards.
 */
function unsupportedReason() {
    if (typeof navigator === "undefined" || !("serviceWorker" in navigator)) {
        return {
            sync: false,
            reason: "no-sw",
            // Firefox private browsing removes the property outright; so does a sandboxed
            // iframe without `allow-same-origin`, and some enterprise policies.
            detail: "navigator.serviceWorker is unavailable in this context",
        };
    }
    if (typeof isSecureContext === "boolean" && !isSecureContext) {
        return {
            sync: false,
            reason: "insecure-context",
            detail: "service workers require a secure context (https, or localhost)",
        };
    }
    return undefined;
}
/**
 * Check the arrangement that actually decides interception.
 *
 * Measured across Blink, Gecko and WebKit: a dedicated worker is controlled because **its own
 * script URL is inside the registration scope**, and whether the *page* is controlled does not
 * matter. So this is the one invariant worth asserting — and it must be asserted, because
 * violating it produces a hang rather than an error.
 */
function checkScope(reg, workerURL) {
    const worker = new URL(workerURL, location.href);
    if (worker.protocol === "blob:") {
        return {
            sync: false,
            reason: "blob-worker",
            detail: "a blob: worker URL is not matched against any scope, so its requests are not intercepted — serve worker.js from a real URL",
        };
    }
    const scope = new URL(reg.scope);
    if (worker.origin !== scope.origin ||
        !worker.pathname.startsWith(scope.pathname)) {
        return {
            sync: false,
            reason: "out-of-scope",
            detail: `the worker script ${worker.pathname} is outside the service worker scope ${scope.pathname}, ` +
                `so its synchronous filesystem requests will not be intercepted — serve sw.js from ` +
                `${new URL(".", worker).pathname} or widen the scope`,
        };
    }
    return undefined;
}
/**
 * Attach `sid` to the service worker so its frames reach `handle`.
 *
 * Re-attaches on its own whenever the worker asks (it was evicted and lost the registry) or the
 * controller changes (it was updated). Both are routine.
 */
async function attachSession(sid, handle, opts) {
    const unsupported = unsupportedReason();
    if (unsupported)
        throw new SyncFsUnavailable(unsupported);
    const reg = await registerOnce(opts.swURL, opts.swScope);
    const scopeProblem = checkScope(reg, opts.workerURL);
    if (scopeProblem)
        throw new SyncFsUnavailable(scopeProblem);
    let prefix;
    let closed = false;
    /** One attempt, over a port of its own. */
    function handshake(active) {
        const { port1, port2 } = new MessageChannel();
        const attached = new Promise((resolve, reject) => {
            const deadline = setTimeout(() => {
                // Closed, or a late `attached` arrives on a channel nobody reads and the
                // service worker keeps a session pointing at a port this page has forgotten.
                port1.close();
                reject(new AttachTimeout());
            }, ATTACH_TIMEOUT_MS);
            port1.onmessage = (event) => {
                const data = event.data;
                if (!data)
                    return;
                if (data.t === "attached") {
                    clearTimeout(deadline);
                    if (data.proto !== WIRE_PROTO) {
                        port1.close();
                        reject(new SyncFsUnavailable({
                            sync: false,
                            reason: "proto-mismatch",
                            detail: `service worker speaks v${data.proto}, this build speaks v${WIRE_PROTO} — reload the page`,
                        }));
                        return;
                    }
                    prefix = new URL(data.prefix, location.href).href;
                    resolve(prefix);
                    return;
                }
                if (data.t === "op") {
                    void answer(data.seq, data.frame, port1);
                }
            };
            port1.start?.();
        });
        // Through `registration.active`, never over a port already held: a `MessagePort` cannot
        // start a stopped service worker, and after an eviction nothing is listening on the old
        // port at all.
        active.postMessage({ t: "attach", sid, proto: WIRE_PROTO }, [port2]);
        return attached;
    }
    /**
     * Attach, and try again if the handshake goes unanswered.
     *
     * A single miss is not evidence of anything wrong. The worker can be stopped, replaced or
     * still starting at the moment the message is posted, and the page cannot see which — the
     * `rescue` and `controllerchange` paths below already treat exactly these as routine and
     * simply re-attach. Only the *first* attach was fatal on one miss, which is the difference
     * between a shell that hiccups and a shell that will not start.
     *
     * `activeOf` is re-read per attempt, so a worker that was replaced between tries is picked
     * up rather than messaged again in its grave.
     */
    async function connect() {
        for (let attempt = 1;; attempt++) {
            const active = await activeOf(reg);
            try {
                return await handshake(active);
            }
            catch (err) {
                // A version mismatch is settled; trying again just spends another five seconds.
                if (!(err instanceof AttachTimeout))
                    throw err;
                if (attempt >= ATTACH_ATTEMPTS) {
                    throw new Error(`service worker did not acknowledge the session after ${ATTACH_ATTEMPTS} ` +
                        `attempts over ${(ATTACH_ATTEMPTS * ATTACH_TIMEOUT_MS) / 1000}s ` +
                        `(${describeWorkerState(reg)})`);
                }
            }
        }
    }
    async function answer(seq, frame, port) {
        // The heartbeat that makes `OP_TIMEOUT_MS` a *liveness* deadline rather than a limit
        // on how long an operation may take.
        //
        // Nothing sent one of these until now, so the service worker's 15 seconds was an
        // absolute ceiling on every synchronous op — including `proc.spawnSync`, whose own
        // contract promises a provider may take as long as it likes, and a blocking read at a
        // prompt, which waits exactly as long as the person at the keyboard does.
        const beat = setInterval(() => {
            try {
                port.postMessage({ t: "progress", seq });
            }
            catch {
                // The port died; the op's own answer will fail the same way.
            }
        }, PROGRESS_INTERVAL_MS);
        let out;
        try {
            // Only the bytes. An op whose reply carries a handle cannot come this way at all —
            // an XHR body has nowhere to put one — which is why ../wire/kinds.ts declares those
            // ops async-only rather than leaving it to be discovered here.
            out = (await handle(frame)).frame;
        }
        catch (err) {
            // The router turns a failed *operation* into an in-band error and never rejects, so
            // reaching here means the handler itself broke. Answer with a message anyway: staying
            // silent leaves the worker parked until a deadline, which turns a bug on this side
            // into an unexplained timeout on the other.
            //
            // Built from the *message*, not from `seq`. `seq` here is the relay's own counter —
            // answering with it produced a reply the worker rejected as a crossed response, so
            // every dispatcher crash was reported as a transport fault instead of itself.
            console.error("[node-worker] dispatch failed", err);
            out = replyToBrokenFrame(frame, err);
        }
        finally {
            clearInterval(beat);
        }
        const buffer = out.buffer.slice(out.byteOffset, out.byteOffset + out.byteLength);
        try {
            port.postMessage({ t: "res", seq, frame: buffer }, [buffer]);
        }
        catch {
            // The port died between the request and the answer — the worker's own deadline covers
            // it.
        }
    }
    prefix = await connect();
    const onServiceWorkerMessage = (event) => {
        const data = event.data;
        if (closed || data?.t !== "rescue" || data.sid !== sid)
            return;
        void connect().catch((err) => console.warn("[node-worker] re-attach after service worker restart failed", err));
    };
    navigator.serviceWorker.addEventListener("message", onServiceWorkerMessage);
    let channel;
    try {
        channel = new BroadcastChannel(SW_BROADCAST_CHANNEL);
        channel.onmessage = onServiceWorkerMessage;
    }
    catch {
        // Not everywhere; the direct message above is the main path.
    }
    // A controller change means the worker was replaced, so the session is attached to
    // something that is gone.
    const onControllerChange = () => {
        if (closed)
            return;
        void connect().catch(() => { });
    };
    navigator.serviceWorker.addEventListener("controllerchange", onControllerChange);
    // Tell the service worker to stop waiting on us the moment this page goes away, so a
    // request in flight fails immediately instead of sitting out the deadline.
    const onPageHide = () => {
        void reg.active?.postMessage({ t: "detach", sid });
    };
    addEventListener("pagehide", onPageHide);
    return {
        prefix,
        detach() {
            closed = true;
            navigator.serviceWorker.removeEventListener("message", onServiceWorkerMessage);
            navigator.serviceWorker.removeEventListener("controllerchange", onControllerChange);
            removeEventListener("pagehide", onPageHide);
            channel?.close();
            reg.active?.postMessage({ t: "detach", sid });
        },
    };
}
/**
 * Synchronous filesystem access is not available, and why.
 *
 * A dedicated error type because the consequence is severe and specific: the module resolver
 * is synchronous end to end, so without this transport there is no `require` and nothing runs.
 * Reporting it as a startup failure naming the cause beats every program failing to resolve its
 * first import.
 */
class SyncFsUnavailable extends Error {
    capabilities;
    constructor(capabilities) {
        super(`node-worker: synchronous filesystem unavailable (${capabilities.reason})` +
            (capabilities.detail ? `: ${capabilities.detail}` : ""));
        this.name = "SyncFsUnavailable";
        this.capabilities = capabilities;
    }
}

// A `ProcessProvider` whose children are `NodeWorker`s.
//
// `NodeWorkerOptions.process` has always described this shape without shipping it — "a second
// `NodeWorker` this page owns is the shape that keeps a command's stdout separate from the
// agent's, which the same worker cannot". This is that, and it is the reason
// `child_process.fork` was ever called structurally impossible: only a page can create a
// `NodeWorker`, so the thing that runs a child has to live here rather than in the worker
// asking for one.
//
// ## What is in here and what is not
//
// Creating the worker is the embedder's, through `start`. Which filesystem it shares, what
// answers *its* `child_process`, whether it keeps a keepalive — those are policy, they differ
// per embedder, and passing them all through would mean re-exporting most of
// `NodeWorkerOptions` for no gain. A pool would be a change to `start` alone.
//
// Everything after the worker exists is in here, because it is mechanism and every piece of it
// is a bug someone would otherwise write again:
//
//   - readers attached *before* the run starts, or the first writes are lost;
//   - `poll` that waits rather than spins, so a silent child costs one message for its life;
//   - stdin closed when the caller says so, and immediately when the caller never will,
//     because a child reading a stdin nobody closes never finishes;
//   - both exit shapes normalised (see `settle`);
//   - `terminate()` on every path, including a failed spawn and `kill`.
/**
 * A running child, from this side.
 *
 * Output accumulates until it is polled for rather than being pushed, because the transport
 * underneath is strictly request-and-reply. The caller keeps exactly one `poll` outstanding and
 * answering it is how anything gets across, so a child that prints nothing costs one message for
 * its whole life.
 */
class Child {
    pid;
    pending = [];
    exited = false;
    wake = null;
    constructor(pid) {
        this.pid = pid;
    }
    push(event) {
        this.pending.push(event);
        this.wake?.();
        this.wake = null;
    }
    /** Whatever has happened since the last ask, waiting until something has. */
    async take() {
        while (this.pending.length === 0 && !this.exited) {
            await new Promise((resolve) => {
                this.wake = resolve;
            });
        }
        return this.pending.splice(0);
    }
}
function enoent(file) {
    return Object.assign(new Error(`spawn ${file} ENOENT`), {
        code: "ENOENT",
        syscall: "spawn",
    });
}
let warnedAboutInherit = false;
function createWorkerProcessProvider(options) {
    const children = new Map();
    let nextPid = 1;
    /** Both exit shapes, normalised by `NodeWorker.settleRun`. */
    async function settle(run) {
        const status = await run.worker.settleRun(run.target, {
            module: run.module,
            argv: run.argv,
            env: run.env,
        });
        return { status, signal: null };
    }
    /** Forward one of the child's output streams into its event queue until it ends. */
    async function pump(stream, onBytes) {
        const reader = stream.getReader();
        try {
            for (;;) {
                const { value, done } = await reader.read();
                if (done)
                    break;
                if (value?.length)
                    onBytes(value);
            }
        }
        catch {
            // The worker went away mid-read. The exit event is what the caller is waiting for.
        }
        finally {
            reader.releaseLock();
        }
    }
    function sinkFor(fd, disposition, child, kind) {
        if (disposition === "ignore")
            return null;
        if (disposition === "inherit") {
            const sink = options.inherit?.(fd);
            if (sink)
                return sink;
            if (!warnedAboutInherit) {
                warnedAboutInherit = true;
                globalThis.console.warn("[node-worker] a child asked for stdio \"inherit\" and this provider has " +
                    "nowhere to put it — pass `inherit` to createWorkerProcessProvider(). " +
                    "The output is being dropped.");
            }
            return null;
        }
        return (bytes) => child.push({ kind, bytes });
    }
    async function begin(request) {
        const run = await options.start(request);
        if (!run)
            throw enoent(request.file);
        const pid = nextPid++;
        const child = new Child(pid);
        const stdio = request.stdio ?? ["pipe", "pipe", "pipe"];
        if (run.source !== undefined) {
            await run.worker.registerVirtualModule(run.target, run.source);
        }
        /*
         * Readers first, and the run started only after. A `TransformStream` buffers, so this is
         * belt and braces rather than a race in practice — but the ordering is free and the
         * failure it prevents (a program's first line missing, sometimes) is the kind that gets
         * blamed on everything else first.
         */
        const out = sinkFor(1, stdio[1], child, "stdout");
        const err = sinkFor(2, stdio[2], child, "stderr");
        const readers = [
            pump(run.worker.console.stdout, (b) => out?.(b)),
            pump(run.worker.console.stderr, (b) => err?.(b)),
        ];
        /*
         * A child whose stdin the caller will never write has to see EOF now. Left open, a
         * program that reads to the end of its input waits for a writer that does not exist,
         * and the run never settles — which presents as a hang rather than as a failure.
         */
        let stdin = null;
        if (stdio[0] === "pipe") {
            stdin = run.worker.console.stdin.getWriter();
        }
        else {
            await run.worker.console.stdin.close().catch(() => { });
        }
        const live = { child, run, stdin, killed: null, done: null };
        live.done = (async () => {
            let status;
            try {
                status = await settle(run);
            }
            catch (err) {
                // The run failed for a reason that is not an exit status at all — the worker
                // died, or the module would not load. Report it as output and a failure, which
                // is what a program crashing looks like from the outside.
                child.push({
                    kind: "stderr",
                    bytes: new TextEncoder().encode(`${err?.stack ?? String(err)}\n`),
                });
                status = { status: 1, signal: null };
            }
            /*
             * Both readers to the end *before* the exit event. The caller stops polling the
             * moment it sees `exit`, so anything pushed after it is discarded — and a reader
             * still inside `read()` when the run settles has not finished. Queueing the exit
             * behind a microtask does not wait for it either: the last chunk loses the race and
             * the command reads as having produced nothing at all.
             */
            try {
                // With a deadline. The run has already settled — the child is gone — so this is
                // waiting for its last bytes to be accepted, and a consumer that has stopped
                // reading must not be able to stop the exit event from ever being pushed.
                await run.worker.console.flushStdio?.(500);
            }
            catch {
                // Best effort; the terminate below is what actually ends the readers.
            }
            try {
                run.worker.terminate();
            }
            catch {
                // Already gone — `process.exit` terminates it for us.
            }
            await Promise.all(readers);
            if (live.killed)
                status = { status: null, signal: live.killed };
            child.exited = true;
            child.push({ kind: "exit", ...status });
            try {
                if (run.source !== undefined) {
                    await run.worker.removeVirtualModule(run.target);
                }
            }
            catch {
                // The vfs may be gone with the worker; nothing to clean up then.
            }
            await run.dispose?.();
            return status;
        })();
        return { pid, live };
    }
    return {
        name: options.name ?? "node-worker",
        async spawn(_ctx, request) {
            const { pid, live } = await begin(request);
            children.set(pid, live);
            void live.done.finally(() => {
                // Kept until the exit event has been collected, which `poll` does.
            });
            return { pid };
        },
        async poll(_ctx, pid) {
            const live = children.get(pid);
            if (!live) {
                throw Object.assign(new Error(`no such process ${pid}`), {
                    code: "ESRCH",
                    syscall: "read",
                });
            }
            const events = await live.child.take();
            if (events.some((e) => e.kind === "exit"))
                children.delete(pid);
            return events;
        },
        async write(_ctx, pid, bytes) {
            const live = children.get(pid);
            if (!live?.stdin)
                return;
            try {
                await live.stdin.write(bytes);
            }
            catch {
                // Writing to a child that has gone is the child's problem, and by the time it
                // can fail here there is nobody left to report it to.
            }
        },
        async endStdin(_ctx, pid) {
            const live = children.get(pid);
            if (!live?.stdin)
                return;
            try {
                await live.stdin.close();
            }
            catch {
                // Already closed, or the worker is gone.
            }
            live.stdin = null;
        },
        async kill(_ctx, pid, signal) {
            const live = children.get(pid);
            if (!live)
                return;
            live.killed = signal;
            // The run's own teardown does the rest: terminate rejects the pending call, `settle`
            // turns that into a status, and `killed` overrides it with the signal.
            try {
                live.run.worker.terminate(new WorkerExitError(-1));
            }
            catch {
                // Already terminated.
            }
        },
        async spawnSync(ctx, request) {
            // The same path, collected. This is the call the architecture is for: the *caller's*
            // worker is parked in a blocking XHR while this side runs the program on its own
            // event loop, so a synchronous spawn can be a whole pipeline rather than only a
            // program that never waits.
            let live;
            let pid;
            try {
                const begun = await begin(request);
                pid = begun.pid;
                live = begun.live;
            }
            catch (err) {
                return {
                    status: null,
                    signal: null,
                    stdout: new Uint8Array(0),
                    stderr: new Uint8Array(0),
                    error: {
                        message: err?.message ?? String(err),
                        code: err?.code,
                    },
                };
            }
            children.set(pid, live);
            if (request.input?.length && live.stdin) {
                await live.stdin.write(request.input);
            }
            if (live.stdin) {
                await live.stdin.close().catch(() => { });
                live.stdin = null;
            }
            const status = await live.done;
            children.delete(pid);
            const out = [];
            const err = [];
            for (const event of live.child.pending) {
                if (event.kind === "stdout")
                    out.push(event.bytes);
                else if (event.kind === "stderr")
                    err.push(event.bytes);
            }
            return {
                status: status.status,
                signal: status.signal,
                stdout: concat(out),
                stderr: concat(err),
            };
        },
    };
}
function concat(chunks) {
    if (chunks.length === 0)
        return new Uint8Array(0);
    if (chunks.length === 1)
        return chunks[0];
    let total = 0;
    for (const c of chunks)
        total += c.length;
    const out = new Uint8Array(total);
    let at = 0;
    for (const c of chunks) {
        out.set(c, at);
        at += c.length;
    }
    return out;
}
/**
 * node's command line, as much of it as a child process needs.
 *
 * Something at the page has to read this: with a shell wired up, a program calling
 * `spawn("node", ["-e", src])` arrives here as argv and nothing else. Returns `null` for a
 * command line this cannot run, which the caller reports as ENOENT.
 */
function nodeCommandLine(args) {
    for (let i = 0; i < args.length; i++) {
        const a = args[i];
        if (a === "-e" || a === "--eval" || a === "-p" || a === "--print") {
            const source = args[i + 1];
            if (source === undefined)
                return null;
            return {
                kind: "eval",
                source,
                print: a === "-p" || a === "--print",
                rest: args.slice(i + 2),
            };
        }
        if (a === "--") {
            const path = args[i + 1];
            if (path === undefined)
                return null;
            return { kind: "script", path, module: moduleOf(path), rest: args.slice(i + 2) };
        }
        // Anything else beginning with "-" is a flag this does not implement. Skipping it
        // rather than failing keeps `node --enable-source-maps script.js` working.
        if (a.startsWith("-") && a !== "-")
            continue;
        return { kind: "script", path: a, module: moduleOf(a), rest: args.slice(i + 1) };
    }
    return null;
}
/** node's own rule: extension first. `.js` is CommonJS absent a package `type`. */
function moduleOf(path) {
    if (path.endsWith(".mjs"))
        return "esm";
    return "cjs";
}

class WorkerExitError extends Error {
    code;
    constructor(code) {
        super(`worker exited with code ${code}`);
        this.code = code;
        this.name = "WorkerExitError";
    }
}
/**
 * How long exit listeners have to read worker-side state before it goes away.
 *
 * Their one window, and not a veto: the program has already ended, so anything still waiting
 * on the run is waiting on this.
 */
const EXIT_LISTENER_GRACE_MS = 1_000;
/**
 * How long a program's own `beforeExit`/`exit` handlers get before the exit is reported anyway.
 *
 * Generous: they legitimately write files, and a slow mount is not a wedged one. What it bounds
 * is the case where they never finish at all, which on this runtime takes the thread with them.
 */
const EXIT_TEARDOWN_GRACE_MS = 10_000;
let workers = 0;
class NodeWorker {
    /**
     * The host-side filesystem. Mount providers on it, populate memory mounts, read them back
     * — all synchronously, since none of it crosses a boundary any more.
     */
    vfs;
    /** Whether synchronous `fs` works, and if not, why. Resolves with `ready`. */
    capabilities;
    worker;
    /**
     * Set by `terminate()`. Checked on both sides of the service-worker await in `ready`,
     * because until the worker exists `terminate()` has nothing to stop — and without this a
     * terminate during startup would be a silent no-op followed by a worker appearing.
     */
    #terminated = false;
    #attachment;
    /** Armed by `ctl.exiting`, cleared by `ctl.exit`. See the handler. */
    #exitDeadline;
    /**
     * The worker's filesystem channel. `port2` is transferred with `init`; this side keeps
     * `port1` and answers frames on it. See {@link VfsInit.port} for why it is separate from
     * the general channel.
     */
    /**
     * The worker's own channel for messages of every kind.
     *
     * Named for the wire rather than the filesystem because it stopped being the
     * filesystem's: process, stdio and control messages ride the same port to the same
     * router. Keeping them off the general worker channel is what stops a reply queueing
     * behind the console output of the very program waiting for it.
     */
    #wireChannel;
    /**
     * What answers `node:child_process`. Absent ⇒ every call throws ENOSYS naming the fix,
     * which is what it did unconditionally before there was an SPI to register.
     */
    #process;
    /**
     * This worker's sync-fs transport session, and deliberately *this worker's* rather than the
     * filesystem's.
     *
     * It namespaces the virtual URLs the blocking XHR posts to — `{syncPrefix}v{proto}/{sid}/{id}-{op}`
     * — where `id` is a request counter each worker starts from zero. Taken from the vfs, two workers
     * sharing one filesystem emitted byte-identical URLs and the service worker answered the second
     * from the first: a sub-worker would ask for its own entry file, be told the right path, and run
     * the previous worker's bytes. A `NodeVfs` is explicitly allowed to back several workers, so the
     * id that separates their traffic cannot be a property of it.
     */
    #syncSid = randomSid();
    /**
     * The exactly-once record for process ops. See ../wire/replay.ts.
     *
     * Held here rather than on the vfs because it belongs to this worker and nothing else: the
     * filesystem's record lives on a `NodeVfs` that may outlive several workers and so has to be
     * forgotten per session, while this one dies with the object that owns it. `proc.spawnSync`
     * is the reason it exists — it is the only process op sent over the retrying transport.
     */
    #procReplies = createReplayCache();
    /** What this worker was built from. See the constructor and `spawnSibling`. */
    #spawnConfig;
    /**
     * What answers a program's `chan.call`, by name. See `registerChannelHandler`.
     *
     * Read at call time rather than captured, so a handler registered after the worker started
     * works — the same reason `#process` is a field rather than a closure.
     */
    #channelHandlers = new Map();
    /**
     * What node-worker answers for itself — thread spawning, today.
     *
     * Separate from `#channelHandlers` and consulted first, so these are available with no
     * embedder setup and an embedder cannot take one of the names by accident.
     */
    #builtinChannels = new Map();
    /** Threads this worker's programs started, by the id they know them under. */
    #threads = new Map();
    #nextThreadId = 1;
    /** This worker's end of the lifecycle channel. Opened on the first spawn. */
    #threadControl;
    /**
     * The kind → dispatcher table, built once and shared by every inbound path.
     *
     * Routing used to be a ternary written out twice — once for the service-worker relay,
     * once for the postMessage path — and the two disagreed: the relay dispatched under
     * `#syncSid` and the other under the vfs's own default, so one worker's synchronous and
     * asynchronous calls landed in two different replay records while sharing a single
     * sequence counter, and `terminate()` only ever closed one of them.
     */
    #wire = new PortEndpoint();
    /**
     * Things the worker asked for that live on **this** side of the boundary.
     *
     * Peer servers and connections, and the fs-events channel: each owns a socket the
     * worker cannot see, and each was designed to be closed by the worker asking — over its
     * port, or by its last watcher leaving. A terminated worker asks for nothing, so every
     * one of them outlived the worker that created it. For a peer server that is worse than
     * a leak: while its signaller socket is open the signaller still has that
     * `(credential, port)` registered, so a `listen(5173)` in the *next* worker competes
     * with a dead one, and a viewer resolving that port can be handed the corpse.
     *
     * Entries drop themselves when they close on their own, so this tracks what is actually
     * live rather than everything ever created.
     */
    #hostResources = new Set();
    /**
     * Unsubscribes for the vfs listeners this worker registered in its constructor.
     *
     * A shared `NodeVfs` outlives the workers on it, so a listener that is never removed is a
     * leak with a live edge back into a terminated worker — and it still runs on every mutation.
     * One dead pair per worker is invisible when a page makes one; a page that spawns a worker
     * per child process makes them by the hundred.
     */
    #unsubscribes = [];
    /** The change feed, while anything in the worker is watching. */
    #events;
    /** Whether `terminate` may dispose of `vfs`, or only end this session on it. */
    #ownsVfs = false;
    exitListeners = new Set();
    ready;
    console;
    /**
     * Register a host-side resource this worker owns, so `terminate` can close it.
     *
     * A resource that reports its own closing (`closed`) deregisters itself, which is what
     * keeps this from growing without bound over a worker that opens many peer
     * connections. Anything that arrives *after* `terminate` — a handshake that was still
     * in flight when the worker died — is closed immediately rather than added, because
     * nothing will ever come back for it.
     */
    #track(resource) {
        if (this.#terminated) {
            try {
                resource.close();
            }
            catch (err) {
                globalThis.console.warn("[node-worker] failed to close a late host resource", err);
            }
            return resource;
        }
        this.#hostResources.add(resource);
        resource.closed?.then(() => this.#hostResources.delete(resource), () => this.#hostResources.delete(resource));
        return resource;
    }
    /**
     * Post to the worker if it is still there.
     *
     * A W2P handler is allowed to terminate the worker — the `exit` one does exactly
     * that — so by the time its reply is ready there may be nothing to reply to. The
     * worker sent that message fire-and-forget for precisely this reason, so dropping
     * the reply is correct; throwing on a detached `worker` would only turn it into an
     * unhandled rejection.
     */
    /**
     * Ask the worker a control question.
     *
     * @internal
     */
    async control(call) {
        return this.#call(call);
    }
    async #call(call, opts) {
        const { decoded } = await this.#wire.call(KIND_CONTROL, call, opts);
        const header = decoded.header;
        if (!header.result.ok)
            throw fromWireError(header.result.error);
        return header.result.value;
    }
    /**
     * Start a worker, awaiting everything that has to be in place first.
     *
     * The recommended entry point, because a constructor cannot reject and a service worker
     * that fails to register is a startup error worth surfacing rather than a filesystem that
     * mysteriously hangs later.
     */
    static async create(workerURL, puterToken, cwd, options) {
        const worker = new NodeWorker(workerURL, puterToken, cwd, options);
        await worker.ready;
        return worker;
    }
    /**
     * `puterToken` may be empty, which starts an **anonymous** worker: nothing here calls
     * api.puter.com, the default filesystem is a memory root with no puterfs under it, and the
     * network comes from `options.net` instead. See `NodeNetInit`.
     */
    constructor(workerURL, puterToken, cwd, options) {
        /*
         * Everything needed to make another worker like this one.
         *
         * Kept rather than destructured and dropped, because "a second worker configured the way
         * the first one was" is the one thing every consumer of a sibling needs and the one thing
         * only the page can do — a worker global has no `navigator.serviceWorker`, so no
         * synchronous filesystem and no module resolver on the other side. See `spawnSibling`.
         */
        this.#spawnConfig = { workerURL, puterToken, cwd, options };
        let keepalive = !!options?.keepalive;
        // No token, no puterfs — `NodeVfs` mounts its memory overlay at "/" on its own when it
        // is given no puter credentials, which is the whole of what an anonymous root is.
        const vfs = options?.vfs ??
            new NodeVfs(puterToken ? { puter: { token: puterToken } } : {});
        this.vfs = vfs;
        // A filesystem this worker made is this worker's to dispose of; one handed in
        // belongs to whoever handed it in and may well outlive several workers. The
        // distinction matters now that a `NodeVfs` holds a change-feed subscription,
        // and with it a socket — disposing a shared one would take that away from
        // every other worker on it.
        this.#ownsVfs = !options?.vfs;
        this.#process = options?.process;
        for (const [name, handler] of Object.entries(options?.channels ?? {})) {
            this.#channelHandlers.set(name, handler);
        }
        this.#installThreadHost();
        // One table, registered once. Both dispatchers run under `#syncSid` so a worker's
        // synchronous and asynchronous calls share one replay record — they share one sequence
        // counter, so anything else splits it.
        this.#wire.router.register(KIND_FS, (frame) => vfs.handleFrame(frame, this.#syncSid));
        this.#wire.router.register(KIND_PROCESS, (frame) => handleProcessFrame(this.#process, frame, {
            cache: this.#procReplies,
            sid: this.#syncSid,
        }));
        // NOT created here. The service worker has to be registered and active *before* the
        // worker script is fetched, because that fetch is when the browser decides whether this
        // worker is controlled — and if it is not, every synchronous `fs` call goes to the
        // network instead of to the filesystem. So creation moves into `ready` below, which
        // every public method already awaits.
        let capabilities;
        this.capabilities = new Promise((r) => (capabilities = r));
        let console = new Console(this, options?.isTTY ?? true);
        this.console = console;
        // Control, in the worker-to-page direction. The other direction — `ctl.init`,
        // `ctl.execute` and friends — is answered by the worker's own dispatcher.
        //
        // There is no `hi` any more. The page used to wait for one before sending `init`,
        // which is a handshake the platform already provides: a `postMessage` to a worker
        // whose script has not finished evaluating is queued, not dropped.
        this.#wire.router.register(KIND_CONTROL, makeDispatcher(KIND_CONTROL, async (msg) => {
            if (msg.op === "ctl.tty") {
                console.handleTTYState({ isRaw: msg.isRaw, echo: msg.echo });
                return;
            }
            if (msg.op === "ctl.exiting") {
                /*
                 * The program said it is on its way out, and its own cleanup runs next.
                 *
                 * That cleanup is synchronous and can block this worker's thread for good,
                 * which would mean no `ctl.exit` ever arrives and a run that never settles.
                 * Nothing inside the worker can bound that. This can: the intent is known,
                 * so silence past the deadline is a wedged teardown rather than a program
                 * still doing its job.
                 *
                 * Armed only by the program declaring an exit, which is what keeps it away
                 * from a worker that is merely parked in a long blocking call — that worker
                 * never said any of this.
                 */
                clearTimeout(this.#exitDeadline);
                this.#exitDeadline = setTimeout(() => {
                    globalThis.console.warn("[node-worker] exit handlers did not finish; terminating");
                    this.terminate(new WorkerExitError(msg.code));
                }, EXIT_TEARDOWN_GRACE_MS);
                return;
            }
            if (msg.op === "ctl.exit") {
                clearTimeout(this.#exitDeadline);
                // The worker is the process, so `process.exit` is the process dying and the
                // worker goes with it. Listeners are awaited *before* the terminate: a
                // consumer whose state lives inside the worker — a memory mount it treats
                // as a replica, say — gets its one chance to read it out here, and there is
                // no second one.
                //
                // Bounded, because that chance must not become a veto. A listener that
                // never settles would leave the worker running and the pending run
                // unsettled — the program is already gone, so what waits is whoever asked
                // for it, for good. A listener that is too slow loses its read; a listener
                // that hangs must not cost the exit.
                await Promise.race([
                    (async () => {
                        for (let listener of [...this.exitListeners]) {
                            try {
                                await listener(msg.code);
                            }
                            catch (err) {
                                // `globalThis`-qualified: the constructor shadows `console`
                                // with the worker's stdio Console, which has no `error`.
                                globalThis.console.error("[node-worker] exit listener failed", err);
                            }
                        }
                    })(),
                    new Promise((resolve) => setTimeout(resolve, EXIT_LISTENER_GRACE_MS)),
                ]);
                this.terminate(new WorkerExitError(msg.code));
                return;
            }
            throw Object.assign(new Error(`control op ${msg.op} is not for the page`), { code: "ENOSYS" });
        }));
        /*
         * Named questions from a program to its host.
         *
         * The mirror of `openChannel`, and the half that was declared and never built. A port is
         * the right shape when the two sides have a protocol to run; this is the right shape for
         * one question with one answer, and it is the only shape that works at all while the
         * worker is parked inside a synchronous call, because a parked worker never reads a port.
         */
        this.#wire.router.register(KIND_CHAN, makeDispatcher(KIND_CHAN, async (call, parts, attachments) => {
            if (call.op !== "chan.open") {
                // Built-ins first, so an embedder cannot shadow thread spawning by
                // registering a handler under one of node-worker's own names.
                const handler = this.#builtinChannels.get(call.name) ??
                    this.#channelHandlers.get(call.name);
                if (!handler) {
                    throw Object.assign(new Error(`no handler for channel "${call.name}" — call ` +
                        `worker.registerChannelHandler(${JSON.stringify(call.name)}, fn), ` +
                        "or pass `channels` to NodeWorker.create"), { code: "ENOSYS" });
                }
                return { value: await handler(call.args, parts, attachments) };
            }
            // `chan.open` travels page → worker; the worker answers it. Arriving here
            // means a frame went the wrong way, which is worth saying rather than
            // silently treating as a question with no handler.
            throw Object.assign(new Error("chan.open is not for the page"), { code: "ENOSYS" });
        }, () => "chan", 
        // Sync-capable, so a retried send must not run the handler again.
        () => ({ cache: this.#procReplies, sid: this.#syncSid })));
        // Stdio. The kind the merge was for: `readSync(0)` and `writeSync(1)` throw EBADF
        // without it, because stdio lived on an envelope that could never be synchronous.
        //
        // Writes normally arrive as *sidebands* on some other message rather than as calls of
        // their own — the router delivers those before the message they rode on, which is what
        // keeps a program's output ahead of the call that carried it.
        this.#wire.router.register(KIND_STDIO, makeDispatcher(KIND_STDIO, async (msg, parts) => {
            if (msg.op === "io.write") {
                console.writeStdio(msg.fd, parts[0]);
                return;
            }
            if (msg.op === "io.flush") {
                await console.flushStdio();
                return;
            }
            const { bytes, eof } = await console.readStdio(msg.length, msg.blocking);
            return { value: { eof }, parts: bytes.length ? [bytes] : undefined };
        }));
        // Peers. Both ops answer with handles rather than values — a stream pair for a
        // connection, a port for a listener — which is what attachments are for and what
        // makes the kind async-only.
        this.#wire.router.register(KIND_PEER, makeDispatcher(KIND_PEER, async (msg) => {
            if (msg.op === "peer.connect") {
                let peer = this.#track(await handlePeerConnect(msg.token, { code: msg.code, port: msg.port }, msg.signaller, msg.ice, msg.anon));
                return {
                    transfer: [
                        peer.readable,
                        peer.writable,
                    ],
                };
            }
            let server = this.#track(await handlePeerServe(msg.token, msg.port, msg.signaller, msg.ice, msg.anon));
            return { value: { code: server.code }, transfer: [server.port] };
        }));
        // Backs node:fs's watchers. The socket lives here rather than in the worker so it's
        // a plain browser WebSocket (the worker's global is epoxy's WISP-tunnelled override)
        // and so one connection serves every watcher across every worker on the token.
        //
        // What the worker gets back is no longer a port. Events are pushed as messages, so
        // they can ride the reply a *parked* worker is already waiting for — which a port
        // could never do, and which is why the runtime used to need a second delivery path
        // for exactly that case.
        this.#wire.router.register(KIND_EVENTS, makeDispatcher(KIND_EVENTS, async (msg) => {
            if (msg.op === "ev.subscribe") {
                this.#events?.close();
                let feed = this.#track(handleFsEvents(msg.token, msg.apiOrigin, (push) => this.#wire.post(KIND_EVENTS, push)));
                this.#events = feed;
                return {
                    value: { connected: feed.connected, polling: feed.polling },
                };
            }
            if (msg.op === "ev.close") {
                this.#events?.close();
                this.#events = undefined;
                return;
            }
            throw Object.assign(new Error(`event op ${msg.op} is not for the page`), { code: "ENOSYS" });
        }));
        this.#wireChannel = new MessageChannel();
        // A tight loop over messages of every kind, and deliberately nothing else. The router
        // answers every failure in band — a message it cannot even parse comes back as a
        // node-shaped error, and one for a kind nobody registered comes back as ENOSYS — so it
        // never rejects, and there is no second error shape for this channel to invent.
        //
        // There used to be one: `{id, error: {message}}`, which is how a dispatcher failure
        // reached the worker with its `code` and `errno` stripped off. The reply is a `WireError`
        // like every other now.
        this.#wireChannel.port1.onmessage = async (e) => {
            const { f } = e.data;
            const out = await this.#answerFrame(f);
            try {
                const envelope = { f: out };
                this.#wireChannel.port1.postMessage(envelope, [out]);
            }
            catch {
                // The port closed between the request and the answer — the worker is going away,
                // and its own deadline covers anything still parked on this.
            }
        };
        // Every local mutation, forwarded to whatever is watching — deliberately including ones
        // this worker caused itself, which it has already seen on their reply frame.
        //
        // Filtering those out reads as the obvious optimization and is a trap: the same
        // `causedBy` covers a host write (an editor save, with no reply to ride) and a sibling
        // worker sharing these providers, so filtering drops exactly the events nothing else
        // delivers. A duplicate costs a redundant rebuild; a drop costs a dev server that has
        // silently stopped noticing edits.
        this.#unsubscribes.push(vfs.onFsEvent((event) => broadcastLocalFsEvent(event)));
        // A mount appearing or disappearing changes answers the worker gives without asking —
        // whether a path's backend has a real positioned read, for one — so re-push it.
        this.#unsubscribes.push(vfs.onMountsChanged((mounts) => {
            if (this.#terminated || !this.worker)
                return;
            this.#call({ op: "ctl.mounts", mounts }).catch(() => {
                // The worker is going away; nothing to tell.
            });
        }));
        this.ready = (async () => {
            if (this.#terminated)
                throw new Error("terminated before start");
            let syncPrefix;
            if (options?.swURL) {
                try {
                    this.#attachment = await attachSession(this.#syncSid, (frame) => this.#wire.router.handle(frame), { swURL: options.swURL, swScope: options.swScope, workerURL });
                    syncPrefix = this.#attachment.prefix;
                }
                catch (err) {
                    if (options.requireSyncFs !== false)
                        throw err;
                    globalThis.console.warn("[node-worker] synchronous filesystem unavailable", err);
                }
            }
            else if (options?.requireSyncFs !== false) {
                throw new SyncFsUnavailable({
                    sync: false,
                    reason: "no-sw",
                    detail: "pass `swURL` (the url of dist/sw.js) to enable synchronous fs",
                });
            }
            // Checked again: registering a service worker is a round trip, and `terminate()`
            // may well have been called during it.
            if (this.#terminated)
                throw new Error("terminated before start");
            // The network, for an anonymous worker. A puter token mints relay
            // credentials of its own and `net` is documented as ignored, so the
            // resolution only runs for the start that actually needs it.
            //
            // Order: what the caller named, then the platform's configured relay,
            // then — only if the host opted in — the third-party public fallback,
            // proven reachable and logged. See src/wire/wisp.ts for the whole
            // argument, including why "none of them" is a startup warning here and a
            // named `WispRelayUnavailable` at the first socket instead.
            let net = options?.net;
            if (!puterToken) {
                const resolved = await resolveWispRelay(options?.net);
                if (resolved) {
                    net = { ...options?.net, wispUrl: resolved.url };
                }
                else {
                    console.warn("[node-worker] no wisp relay configured — node:net / node:tls will throw " +
                        "WispRelayUnavailable on first use. Pass net: { wispUrl }, or have the " +
                        "platform publish one (HIVE_BROWSER_WISP_URL, written here as " +
                        "globalThis.HIVE_WISP_URL).");
                }
            }
            this.worker = new Worker(workerURL, {
                name: "node-worker-" + workers++,
                type: "module",
            });
            // The bootstrap, and the only message that does not go over the port — it is what
            // delivers the port, and the port is now its only attachment. Posted without
            // waiting for the worker to announce itself, because a message to a worker whose
            // script is still evaluating is queued rather than dropped.
            this.#wire.attach(this.#wireChannel.port1);
            let settled = await this.#wire.bootstrap(this.worker, KIND_CONTROL, {
                op: "ctl.init",
                puter: puterToken ?? "",
                net,
                epoxyBase: options?.epoxyBase,
                cwd,
                keepalive,
                isTTY: console.isTTY,
                vfs: {
                    sid: this.#syncSid,
                    proto: WIRE_PROTO,
                    syncPrefix,
                    timeoutMs: options?.syncTimeoutMs ?? SYNC_TIMEOUT_MS,
                    mounts: vfs.snapshot(),
                },
            }, [this.#wireChannel.port2]);
            let init = settled.decoded.header;
            if (!init.result.ok)
                throw fromWireError(init.result.error);
            let reply = init.result.value;
            capabilities(reply.capabilities);
            if (!reply.capabilities.sync && options?.requireSyncFs !== false) {
                throw new SyncFsUnavailable(reply.capabilities);
            }
        })();
        // Nothing necessarily awaits `capabilities` if `ready` rejected first.
        this.ready.catch(() => capabilities({ sync: false, reason: "probe-failed" }));
    }
    /**
     * One message, answered.
     *
     * A fresh `ArrayBuffer` rather than a view, because it is transferred back and a view into a
     * larger buffer would send the whole thing.
     */
    async #answerFrame(frame) {
        const out = (await this.#wire.router.handle(frame)).frame;
        return out.buffer.slice(out.byteOffset, out.byteOffset + out.byteLength);
    }
    /**
     * Register what runs programs, after construction.
     *
     * The counterpart of `NodeWorkerOptions.process`, and useful for the same reason
     * `mount()` is: a shell often needs the worker's own filesystem to exist first, and a
     * provider that relays to a second worker cannot be built before this one is started.
     */
    registerProcessProvider(provider) {
        this.#process = provider;
    }
    /**
     * Answer a program's `chan.call` for one name.
     *
     * The worker side is:
     *
     *   const chan = require("node-worker/channel");
     *   const answer = await chan.call("phx.suite", results);
     *   const answer = chan.callSync("phx.suite", results);   // works while parked
     *
     * `args` is whatever the two sides agreed on and `parts` carries any bytes. Returning a value
     * answers; throwing answers with the error, and `err.code` survives the crossing.
     *
     * A handler may be called more than once for one logical request: a synchronous send retries
     * the same `seq` after a transport failure. The reply record answers a repeat without running
     * the handler again, so that is handled — but a handler that starts work of its own and
     * returns before it finishes has stepped outside that guarantee.
     *
     * Returns a function that removes it.
     */
    registerChannelHandler(name, handler) {
        this.#channelHandlers.set(name, handler);
        return () => {
            if (this.#channelHandlers.get(name) === handler) {
                this.#channelHandlers.delete(name);
            }
        };
    }
    /**
     * Hand a program running in this worker a `MessagePort`, under a name it can ask for.
     *
     * The page and a program otherwise have only the console streams between them, which is a
     * byte pipe carrying whatever the program prints — fine for output, and a poor place to put
     * a control protocol. The worker side is:
     *
     *   const port = await require("node-worker/channel").channel("shell");
     *
     * Opening a channel the program never asks for is harmless; asking for one the page never
     * opens waits, on the reasoning that a program waiting for its host is not an error.
     */
    async openChannel(name) {
        const channel = new MessageChannel();
        await this.openChannelWith(name, channel.port2);
        return channel.port1;
    }
    /**
     * Hand this worker a port the *caller* made, rather than one minted here.
     *
     * `openChannel` keeps the other end, which is right when the page is one of the two parties.
     * It is wrong when the two parties are two workers: opening a channel on each would leave the
     * page relaying every message between them, which costs a hop each way and re-transfers every
     * transferable through a third realm. With this, the page makes one `MessageChannel` and gives
     * an end to each worker — after which they talk directly and the page is not in the data path
     * at all. That is what `worker_threads` needs to be worth having.
     */
    async openChannelWith(name, port) {
        await this.ready;
        await this.#wire.call(KIND_CHAN, { op: "chan.open", name }, { transfer: [port] });
    }
    /**
     * Another worker configured the way this one was.
     *
     * Only a page can create a `NodeWorker`, so anything that wants a child process, a worker
     * thread or a second realm has to come back here for it — and every one of them wants the
     * same thing: the same worker script, the same service worker, the same filesystem, the same
     * network. Rebuilding that by hand at each call site is how one of them ends up with a
     * private `NodeVfs` and a child that cannot see the files its parent just wrote.
     *
     * The filesystem is inherited **by reference**, deliberately: siblings share a namespace, so
     * a guest path means the same thing in both. `vfs` in `overrides` opts out.
     *
     * The caller owns what comes back and must `terminate()` it. A normal return leaves a worker
     * running — see `settleRun`.
     */
    async spawnSibling(overrides) {
        const { workerURL, puterToken, cwd, options } = this.#spawnConfig;
        const { cwd: cwdOverride, ...optionOverrides } = overrides ?? {};
        return NodeWorker.create(workerURL, puterToken, cwdOverride ?? cwd, {
            ...options,
            // The filesystem this worker actually ended up on, which is not the same as
            // `options.vfs`: a worker given none built its own, and a sibling that built a
            // second one would share nothing with it.
            vfs: this.vfs,
            ...optionOverrides,
        });
    }
    /**
     * Run a module in this worker and answer with its exit code, whichever way it ends.
     *
     * There are two shapes and only one of them looks like an ending. A program that returns
     * normally resolves `require`/`import` with its code and **leaves this worker running** —
     * nothing tears it down, so a caller that forgets to `terminate()` leaks one per run. A
     * program that calls `process.exit` gets there the other way: the worker posts `ctl.exit`,
     * the page terminates it, and the pending call *rejects* with `WorkerExitError` carrying the
     * status. Treating that rejection as a failure is how `node -e 'process.exit(3)'` turns into
     * a crash report instead of an exit status, which is a mistake worth making once.
     */
    async settleRun(target, options) {
        try {
            return options?.module === "esm"
                ? await this.import(target, options)
                : await this.require(target, options);
        }
        catch (err) {
            if (err instanceof WorkerExitError)
                return err.code;
            throw err;
        }
    }
    /**
     * What answers `worker_threads.Worker` for programs in this worker.
     *
     * Registered unconditionally, which is the point: only a page can create a `NodeWorker`, so
     * without this every `new Worker(...)` in every worker throws no matter what the embedder
     * does. The pieces it needs — `spawnSibling`, `openChannelWith`, `settleRun` — are the same
     * ones a `ProcessProvider` uses; what differs is that the data path is a real `MessagePort`
     * between the two workers rather than anything on the wire.
     */
    #installThreadHost() {
        this.#builtinChannels.set("nw:thread.spawn", async (args, _parts, attachments) => {
            const a = args;
            const id = this.#nextThreadId++;
            const child = await this.spawnSibling();
            /*
             * One channel, an end to each worker. `openChannel` would have kept an end here and
             * left this page relaying every message between them — a hop each way, and every
             * transferable re-transferred through a third realm. With both ends placed, the two
             * workers talk directly and nothing here sees their traffic.
             */
            const pair = new MessageChannel();
            await child.openChannelWith("nw:parentPort", pair.port2);
            await this.openChannelWith(`nw:thread:${id}`, pair.port1);
            // Lifecycle and, if asked for, output. Opened lazily and once — a worker that never
            // starts a thread never gets one.
            if (!this.#threadControl) {
                const ctl = new MessageChannel();
                await this.openChannelWith("nw:threads", ctl.port2);
                this.#threadControl = ctl.port1;
                this.#threadControl.start();
            }
            const control = this.#threadControl;
            const readers = [];
            const forward = (stream, type) => readers.push((async () => {
                const reader = stream.getReader();
                try {
                    for (;;) {
                        const { value, done } = await reader.read();
                        if (done)
                            break;
                        if (value?.length)
                            control.postMessage({ id, type, bytes: value });
                    }
                }
                catch {
                    // The child went away; `exit` is what the parent is waiting for.
                }
                finally {
                    reader.releaseLock();
                }
            })());
            // Asked for, or nowhere to put it. A thread whose output nobody requested writes to
            // this page's console the way node pipes a worker's stdout to its parent's.
            if (a.stdout)
                forward(child.console.stdout, "stdout");
            if (a.stderr)
                forward(child.console.stderr, "stderr");
            this.#threads.set(id, child);
            void (async () => {
                let code = 0;
                try {
                    code = await child.settleRun(a.target, {
                        module: a.target.endsWith(".mjs") ? "esm" : "cjs",
                        argv: a.argv ?? ["node", a.target],
                        env: a.env,
                        thread: {
                            threadId: id,
                            workerData: attachments[0],
                            portChannel: "nw:parentPort",
                            ipc: !!a.ipc,
                        },
                    });
                }
                catch (err) {
                    control.postMessage({
                        id,
                        type: "error",
                        message: err?.message ?? String(err),
                        stack: err?.stack,
                    });
                    code = 1;
                }
                // Exit last, after the readers have ended: the parent stops listening for this
                // id once it sees it, so anything posted afterwards is discarded — including the
                // last chunk a reader was still inside `read()` for.
                try {
                    child.terminate();
                }
                catch {
                    // `process.exit` already terminated it.
                }
                await Promise.all(readers);
                this.#threads.delete(id);
                control.postMessage({ id, type: "exit", code });
            })();
            return { id, threadId: id };
        });
        this.#builtinChannels.set("nw:thread.terminate", async (args) => {
            const { id } = args;
            this.#threads.get(id)?.terminate(new WorkerExitError(1));
            return null;
        });
    }
    async setCwd(cwd) {
        await this.ready;
        await this.#call({ op: "ctl.cwd", cwd });
    }
    // -------------------------------------------- the filesystem, from the host
    //
    // These all used to be page↔worker messages. They are ordinary calls into `this.vfs` now,
    // which means they are **synchronous underneath** — the `async` signatures are kept only so
    // existing callers do not have to change. Reach for `worker.vfs` directly for the synchronous
    // forms and for anything the old message set could not express (mounting your own provider,
    // listing mounts, watching for changes).
    //
    //   const proj = worker.vfs.mountMemory("/proj");
    //   proj.write([
    //     { path: "package.json", data: pkgJson },
    //     { path: "src/main.js",  data: src },
    //   ]);
    //   await worker.setCwd("/proj");
    //   await worker.import("/proj/src/main.js");
    async registerVirtualModule(path, code) {
        this.vfs.addVirtualFile(path, code);
    }
    async removeVirtualModule(path) {
        this.vfs.removeVirtualFile(path);
    }
    /**
     * Create a memory-backed directory at `root`.
     *
     * `replace` swaps out an existing mount at the same root instead of throwing, which is what
     * re-populating a project between runs wants.
     */
    async mountMemory(root, options) {
        this.vfs.mountMemory(root, options);
    }
    async unmountMemory(root) {
        this.vfs.unmountMemory(root);
    }
    /**
     * Write entries into the memory mount at `root`, or into the overlay over the root mount when
     * `root` is "/".
     *
     * Entry paths are relative to the mount root, and files create their own parent directories —
     * an entry with no `data` is only needed for a deliberately empty one. Strings are encoded as
     * UTF-8.
     *
     * `options.transfer` is accepted and **ignored**. It used to hand the underlying buffers to
     * the worker instead of copying them, and it detached every `Uint8Array` and `ArrayBuffer`
     * you passed. There is no boundary to cross any more, so the copy it was avoiding is a single
     * local one and the hazard is simply gone.
     */
    async writeMemory(root, files, options) {
        return this.vfs.memory(root).write(files);
    }
    /** Remove paths (relative to `root`) from a memory mount. */
    async removeMemory(root, paths) {
        this.vfs.memory(root).remove(paths);
    }
    /**
     * Read one file out of a memory mount, or `undefined` if the path is absent or a directory.
     * `path` is relative to `root`.
     */
    async readMemory(root, path) {
        return this.vfs.memory(root).read(path);
    }
    /**
     * List a directory in a memory mount, or `undefined` if the path is absent or a file. `path`
     * is relative to `root`; `""` and `"/"` both mean the root itself.
     *
     * `since` reports only what was modified after that time, which is what makes "what did this
     * run touch?" cheap even over a tree with a `node_modules` in it.
     */
    async listMemory(root, path, options) {
        return this.vfs.memory(root).list(path, options);
    }
    /**
     * Run `path` as CommonJS and resolve with its exit code.
     *
     * Rejects with `WorkerExitError` if the program called `process.exit`, which also
     * terminates the worker — read `err.code` for the status.
     */
    async require(path, options) {
        return this.execute("cjs", path, options);
    }
    /** As `require`, but run `path` as an ES module. */
    async import(path, options) {
        return this.execute("esm", path, options);
    }
    async execute(module, target, options) {
        await this.ready;
        let reply = await this.#call({
            op: "ctl.execute",
            module,
            target,
            argv: options?.argv,
            env: options?.env,
            thread: options?.thread && {
                threadId: options.thread.threadId,
                portChannel: options.thread.portChannel,
                ipc: options.thread.ipc,
            },
        }, 
        // Cloned beside the call rather than encoded into it: the header is JSON, and
        // `workerData` is whatever structured clone can carry.
        options?.thread ? { attach: [options.thread.workerData] } : undefined);
        return reply.exitCode;
    }
    /**
     * Called when the worker exits of its own accord, before it is terminated.
     *
     * A listener returning a promise is awaited, which is the only window in which
     * worker-side state can still be read. Returns an unsubscribe function.
     */
    onExit(listener) {
        this.exitListeners.add(listener);
        return () => this.exitListeners.delete(listener);
    }
    /**
     * Stop the worker. Idempotent.
     *
     * Every request still in flight is rejected with `reason`, because a terminated
     * worker will never answer one. That matters most for the `execute` of a program
     * that just called `process.exit`: without this it would stay pending forever, and
     * the caller would be left waiting on a run that has already finished.
     */
    terminate(reason) {
        // Set first, and independently of whether the worker exists yet: creation is deferred
        // behind service-worker registration, so `terminate()` during startup has nothing to
        // stop — and without this flag it would be a silent no-op followed by a worker
        // appearing anyway.
        if (this.#terminated)
            return;
        this.#terminated = true;
        // Tell the service worker to stop relaying for this session, so a request in flight
        // fails immediately rather than sitting out its deadline.
        this.#attachment?.detach();
        this.#attachment = undefined;
        // Peer servers and connections, and the fs-events channel. All of these are closed
        // by the worker *asking*, and the worker is about to stop being able to ask — see
        // `#hostResources`. A peer server in particular has to go now rather than whenever
        // the page unloads, because the signaller keeps its port registered for exactly as
        // long as its socket is open.
        let resources = [...this.#hostResources];
        this.#hostResources.clear();
        for (let resource of resources) {
            try {
                resource.close();
            }
            catch (err) {
                globalThis.console.warn("[node-worker] failed to close a host resource", err);
            }
        }
        // The vfs listeners this worker added. Same reasoning as the resources above: a shared
        // filesystem outlives the worker, so what the constructor took here has to be given back.
        let unsubscribes = this.#unsubscribes;
        this.#unsubscribes = [];
        for (let unsubscribe of unsubscribes) {
            try {
                unsubscribe();
            }
            catch (err) {
                globalThis.console.warn("[node-worker] failed to remove a vfs listener", err);
            }
        }
        // The host owns this session's open files, and they outlive the worker unless dropped —
        // which for a memory mount means leaking the contents of unlinked files, kept alive on
        // purpose for exactly as long as a handle refers to them. Dirty buffers are deliberately
        // not flushed: a worker that died did not ask for its pending writes to be published.
        //
        // A filesystem this worker created goes further and is disposed of outright,
        // since nothing else can be holding it — that also releases its change-feed
        // subscription, which would otherwise keep a socket open for the life of the
        // page. See `#ownsVfs`.
        if (this.#ownsVfs)
            this.vfs.dispose();
        else
            this.vfs.closeSession(this.#syncSid);
        // The filesystem channel outlives the worker otherwise: a port with a live `onmessage`
        // keeps this side reachable, and the handler closes over the vfs that was just disposed
        // of above.
        this.#wireChannel?.port1.close();
        this.#wireChannel = undefined;
        this.worker?.terminate();
        this.worker = undefined;
        // End stdout/stderr, so a page reading them stops waiting. Nothing will write again, and
        // a `TransformStream` readable only ends when this side closes its writable — so without
        // this, "read the output until it ends" never returns. Not awaited: `terminate` is
        // synchronous, and the close only has queued writes ahead of it.
        void this.console.closeStdio().catch(() => { });
        let error = reason ?? new Error("Worker terminated");
        // One call, where there used to be a hand-rolled drain of an inflight map that the
        // worker's own half never had at all — so a terminated worker left its side parked
        // forever on promises nothing would settle.
        this.#wire.close(error);
        this.ready = Promise.reject(error);
        // Nothing necessarily awaits the replacement `ready`, and an unobserved
        // rejected promise is a console warning in every browser.
        this.ready.catch(() => { });
    }
}

export { Console, NodeVfs, NodeWorker, PUBLIC_WISP_FALLBACK_URL, SyncFsUnavailable, VfsError, WispRelayUnavailable, WorkerExitError, createCachingProvider, createDirectoryHandleProvider, createMemoryProvider, createPuterProvider, createWorkerProcessProvider, ensureDirectoryHandleAccess, fsError, isWispUrl, nodeCommandLine, platformWispUrl, publicFallbackAllowed, publicFallbackUrl, resolveWispRelay, unionProvider };
