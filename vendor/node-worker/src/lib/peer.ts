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

function rtcDataChannelToStreams(
	pc: RTCPeerConnection,
	dc: RTCDataChannel,
	{
		writeHighWaterMark = 1 << 20, // 1 MiB
		writeLowWaterMark = writeHighWaterMark >> 1,
		maxPendingReadBytes = 1 << 20, // hard cap; true inbound backpressure needs app-level flow control
	}: {
		writeHighWaterMark?: number;
		writeLowWaterMark?: number;
		maxPendingReadBytes?: number;
	} = {}
): [
	ReadableStream<Uint8Array<ArrayBuffer>>,
	WritableStream<Uint8Array<ArrayBuffer>>,
	Promise<void>,
] {
	dc.binaryType = "arraybuffer";
	dc.bufferedAmountLowThreshold = writeLowWaterMark;

	const channelError = () =>
		new DOMException("RTCDataChannel is not open", "NetworkError");

	const waitForOpen = () =>
		dc.readyState === "open"
			? Promise.resolve()
			: new Promise<void>((resolve, reject) => {
					const onOpen = () => done(resolve);
					const onClose = () =>
						done(() => {
							console.warn(
								"[node-worker] [peer] [rtc] datachannel closed before open"
							);
							reject(channelError());
						});
					const onError = (e: Event) =>
						done(() => {
							console.warn(
								"[node-worker] [peer] [rtc] datachannel errored before open",
								e
							);
							reject(channelError());
						});
					const done = (fn: () => void) => {
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
		if (!negotiated || !Number.isFinite(negotiated)) return SAFE_MESSAGE_SIZE;
		return Math.max(1, Math.min(negotiated, SAFE_MESSAGE_SIZE));
	};

	const waitForWritable = () =>
		dc.bufferedAmount <= writeLowWaterMark
			? Promise.resolve()
			: new Promise<void>((resolve, reject) => {
					const onLow = () => done(resolve);
					const onClose = () =>
						done(() => {
							console.warn(
								"[node-worker] [peer] [rtc] datachannel closed while waiting to drain"
							);
							reject(channelError());
						});
					const onError = (e: Event) =>
						done(() => {
							console.warn(
								"[node-worker] [peer] [rtc] datachannel errored while waiting to drain",
								e
							);
							reject(channelError());
						});
					const done = (fn: () => void) => {
						dc.removeEventListener("bufferedamountlow", onLow);
						dc.removeEventListener("close", onClose);
						dc.removeEventListener("error", onError);
						fn();
					};
					dc.addEventListener("bufferedamountlow", onLow, { once: true });
					dc.addEventListener("close", onClose, { once: true });
					dc.addEventListener("error", onError, { once: true });
				});

	let readController: ReadableStreamDefaultController<
		Uint8Array<ArrayBuffer>
	> | null = null;
	let readClosed = false;
	let pending: Uint8Array<ArrayBuffer>[] = [];
	let pendingBytes = 0;

	const maybeCloseReadable = () => {
		if (readClosed && pending.length === 0 && readController) {
			try {
				readController.close();
			} catch (e) {
				console.warn(
					"[node-worker] [peer] [rtc] failed to close readable (likely already errored/cancelled)",
					e
				);
			}
			readController = null;
		}
	};

	const drainReads = () => {
		if (!readController) return;
		try {
			while (pending.length && (readController.desiredSize ?? 0) > 0) {
				const chunk = pending.shift()!;
				pendingBytes -= chunk.byteLength;
				readController.enqueue(chunk);
			}
		} catch (e) {
			console.warn(
				"[node-worker] [peer] [rtc] failed to enqueue inbound chunk",
				e
			);
			readController = null;
			return;
		}
		maybeCloseReadable();
	};

	dc.addEventListener("message", (event) => {
		const chunk = new Uint8Array(event.data as ArrayBuffer);
		pending.push(chunk);
		pendingBytes += chunk.byteLength;

		if (pendingBytes > maxPendingReadBytes) {
			const err = new DOMException(
				"Readable side overflowed; RTCDataChannel cannot apply true inbound backpressure without app-level flow control",
				"QuotaExceededError"
			);
			console.warn("[node-worker] [peer] [rtc] inbound overflow", err);
			try {
				readController?.error(err);
			} catch (e) {
				console.warn(
					"[node-worker] [peer] [rtc] failed to error readable on overflow",
					e
				);
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
		} catch (e) {
			console.warn(
				"[node-worker] [peer] [rtc] failed to propagate error to readable",
				e
			);
		}
		readController = null;
	});

	const readable = new ReadableStream<Uint8Array<ArrayBuffer>>(
		{
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
		},
		{
			highWaterMark: writeHighWaterMark,
			size: (chunk) => chunk.byteLength,
		}
	);

	const writable = new WritableStream<Uint8Array<ArrayBuffer>>({
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
				} catch (e) {
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
function credential(token: string, anon: boolean | undefined) {
	return anon ? { anonToken: token } : { authToken: token };
}

/**
 * A peer resource that lives on **this** side of the worker boundary.
 *
 * Every one of these owns a signaller socket and at least one `RTCPeerConnection`, none
 * of which the worker can reach, let alone close. So each hands back the means to shut it
 * down from here — `close`, idempotent — and a `closed` that settles however the teardown
 * happened, so an owner tracking these can stop tracking one that let go on its own.
 */
export interface PeerHandle {
	close(): void;
	closed: Promise<void>;
}

export interface PeerServeHandle extends PeerHandle {
	/** The invite code, or "" for an anonymous server; see the resolve below. */
	code: string;
	/** Transferred to the worker: each accepted connection arrives as a message. */
	port: MessagePort;
}

export interface PeerConnectHandle extends PeerHandle {
	readable: ReadableStream<Uint8Array<ArrayBuffer>>;
	writable: WritableStream<Uint8Array<ArrayBuffer>>;
}

export async function handlePeerServe(
	token: string,
	port: number,
	signaller: string,
	iceServers: RTCIceServer[],
	anon?: boolean
): Promise<PeerServeHandle> {
	let conns = new Map<string, RTCPeerConnection>();
	let code = `<port ${port}>`;

	let { port1: rx, port2: tx } = new MessageChannel();
	tx.start();

	let ws = new WebSocket(signaller);

	let settleClosed: () => void;
	let closed = new Promise<void>((res) => (settleClosed = res));
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
		if (done) return;
		done = true;
		for (let [, peer] of conns) peer.close();
		conns.clear();
		tx.close();
		ws.close();
		settleClosed();
	};

	try {
		await new Promise<void>((res, rej) => {
			ws.onopen = () => res();
			ws.onerror = (e) => {
				console.warn("[node-worker] [peer] signaller error", e);
				rej(new Error("Signaller connection errored unexpectedly"));
			};
			ws.onclose = () =>
				rej(new Error("Signaller connection closed unexpectedly"));
		});

		ws.send(
			JSON.stringify({
				server: {
					create: {
						...credential(token, anon),
						port,
					},
				},
			})
		);

		let resolve = (_: any): void => {
			throw "unreachable";
		};

		ws.onmessage = async (e) => {
			let msg = JSON.parse(e.data).server;
			if (!msg) return;

			if (msg.create) {
				resolve(msg.create);
			} else if (msg.connect) {
				try {
					let id = msg.connect.id;
					let peer = new RTCPeerConnection({ iceServers });
					conns.set(id, peer);

					// A server outlives the connections it accepts, and `conns` is what
					// `shutdown` closes — so without this, every client that ever
					// disconnected stays in the map holding an ICE/TURN allocation open for
					// as long as the server runs.
					peer.addEventListener("connectionstatechange", () => {
						if (
							peer.connectionState !== "closed" &&
							peer.connectionState !== "failed"
						) {
							return;
						}
						if (conns.get(id) === peer) conns.delete(id);
						peer.close();
					});

					peer.onicecandidate = (e) => {
						if (!e.candidate) return;
						ws.send(
							JSON.stringify({
								server: {
									candidate: {
										id,
										candidate: e.candidate,
									},
								},
							})
						);
					};

					let datachannel = peer.createDataChannel("channel-1", {
						negotiated: true,
						id: 2,
					});
					let [readable, writable, ready] = rtcDataChannelToStreams(
						peer,
						datachannel,
						{ maxPendingReadBytes: Infinity }
					);
					await ready;
					tx.postMessage(
						{ readable, writable },
						{ transfer: [readable, writable] }
					);
				} catch (err) {
					console.warn("[node-worker] [peer] failed to accept client", err);
				}
			} else if (msg.candidate) {
				let peer = conns.get(msg.candidate.id);
				if (!peer) return;

				await peer.addIceCandidate(msg.candidate.candidate);
			} else if (msg.offer) {
				let id = msg.offer.id;
				let peer = conns.get(id);
				if (!peer) return;

				await peer.setRemoteDescription(
					new RTCSessionDescription(msg.offer.offer)
				);
				let answer = await peer.createAnswer();
				await peer.setLocalDescription(answer);

				ws.send(
					JSON.stringify({
						server: {
							answer: {
								id,
								answer,
							},
						},
					})
				);
			}
		};

		code = await new Promise<string>((res, rej) => {
			resolve = (data) => {
				if (data.success) {
					// An invite code is how an *authenticated* server is reached, and the
					// signaller only mints one for that case — an anonymous create answers a
					// bare `{success:true}`, because its address is the `(anonToken, port)`
					// pair the client already has. So a missing code is success, not a
					// half-created server, and the caller keeps it only to report it.
					res(data.invitecode ?? "");
				} else {
					rej(new Error(`Signaller failed: ${data.error}`));
				}
			};
			setTimeout(() => rej(new Error("Server creation timed out")), 15000);
		});

		ws.onerror = (e) =>
			console.warn("[node-worker] [peer] signaller error", code, e);
		ws.onclose = () =>
			console.warn("[node-worker] [peer] signaller closed", code);

		tx.onmessage = () => shutdown();

		return { code, port: rx, close: shutdown, closed };
	} catch (e) {
		shutdown();
		throw e;
	}
}

export async function handlePeerConnect(
	token: string,
	target: { code?: string; port?: number },
	signaller: string,
	iceServers: RTCIceServer[],
	anon?: boolean
): Promise<PeerConnectHandle> {
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

	let settleClosed: () => void;
	let closed = new Promise<void>((res) => (settleClosed = res));
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
		if (done) return;
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
		if (code) code = code.toUpperCase();
		console.debug(
			"[node-worker] [peer] dialing",
			target.port != null ? `port ${target.port}` : `invite code ${code}`
		);
		await new Promise<void>((res, rej) => {
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
		let wsErrorPromise = new Promise<void>((_, rej) => {
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
		ws.send(
			JSON.stringify({
				client: {
					connect: {
						...credential(token, anon),
						...(target.port != null
							? { port: target.port }
							: { invitecode: code }),
					},
				},
			})
		);

		peer.onicecandidate = (e) => {
			if (!e.candidate) return;
			ws.send(
				JSON.stringify({
					client: {
						candidate: {
							candidate: e.candidate,
						},
					},
				})
			);
		};

		let wsPromise = new Promise<void>((_, rej) => {
			ws.onmessage = async (e) => {
				let msg = JSON.parse(e.data).client;
				if (!msg) return;

				if (msg.answer) {
					await peer.setRemoteDescription(msg.answer.answer);
				} else if (msg.candidate) {
					if (msg.candidate.candidate) {
						await peer.addIceCandidate(msg.candidate.candidate);
					}
				} else if (msg.connect) {
					if (msg.connect.success) {
						let offer = await peer.createOffer();
						await peer.setLocalDescription(offer);
						ws.send(
							JSON.stringify({
								client: {
									offer: { offer },
								},
							})
						);
					} else {
						rej(new Error(`Signaller failed: ${msg.connect.error}`));
					}
				} else if (msg.disconnect) {
					rej(
						new Error(`Signaller sent a disconnect: ${msg.disconnect.reason}`)
					);
				}
			};
		});

		await Promise.race([ready, wsPromise, wsErrorPromise]);
		return { readable, writable, close: shutdown, closed };
	} catch (e) {
		shutdown();
		throw e;
	}
}
