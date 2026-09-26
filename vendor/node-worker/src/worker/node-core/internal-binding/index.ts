// JS replacement for node's `internalBinding(name)` C++ bridge. Resolves the
// names that bundled `node_core/lib/*.js` modules ask for. Anything unmapped
// throws so we notice at runtime rather than silently producing undefined.
//
// To support a new upstream module, add an entry here. Throwing C++-shaped
// stubs (tcp_wrap, pipe_wrap, ...) belong in `./stubs.ts`.

import zlibBinding from "./zlib";
import constants from "./constants";
import streamWrap from "./stream_wrap";
import uv from "./uv";
import modules from "./modules";
import url from "./url";
import urlPattern from "./url_pattern";
import encodingBinding from "./encoding_binding";
import utilBinding from "./util";
import bufferBinding from "./buffer";
import fs from "./fs";
import httpParser from "./http_parser/index.js";
import cryptoBinding from "./crypto";
import http2Binding from "./http2/index";
import traceEvents from "./trace_events";
import types from "./types";
import config from "./config";
import timers from "./timers";
import asyncContextFrame from "./async_context_frame";
import contextify from "./contextify";

// Resolved lazily, in two stages, because this module is unavoidably inside a
// cycle: `./crypto` and `./http2` pull in node-core lib modules that reach
// `internal/util/inspect`, whose own top level calls `internalBinding('util')`.
// A plain `const bindings = {...}` throws "Cannot access 'bindings' before
// initialization" whenever rollup happens to order that caller first — and which
// side wins shifts with any change to the fs or stream subgraphs.
//
// So: the lookup object is built on first call (not at module scope), and each
// entry is a thunk, so a binding module still in TDZ is only read when that
// specific binding is asked for. Every binding needed during bootstrap (util,
// types, constants, trace_events, timers, async_context_frame, uv) is a leaf
// module that sorts before this one; the heavy ones (crypto, http2, zlib) are
// only ever requested long after.
let lookup: Record<string, () => any> | undefined;

function buildLookup(): Record<string, () => any> {
	return {
		zlib: () => zlibBinding,
		constants: () => constants,
		stream_wrap: () => streamWrap,
		timers: () => timers,
		// AsyncContextFrame (upstream internal/async_context_frame.js) destructures
		// get/setContinuationPreservedEmbedderData at load and uses them as
		// current()/set(). `--async-context-frame` is enabled (see lib/internal/
		// options.js), so these are live: they read/write the shared holder that the
		// await-transform and microtask patches also mutate. See ./async_context_frame.
		async_context_frame: () => asyncContextFrame,
		uv: () => uv,
		modules: () => modules,
		url: () => url,
		url_pattern: () => urlPattern,
		encoding_binding: () => encodingBinding,
		util: () => utilBinding,
		// A leaf like `util`, so it is safe to resolve during bootstrap —
		// `internal/util/comparisons` reaches for it at its own top level.
		buffer: () => bufferBinding,
		fs: () => fs,
		http_parser: () => httpParser,
		crypto: () => cryptoBinding,
		http2: () => http2Binding,
		trace_events: () => traceEvents,
		types: () => types,
		config: () => config,
		// A leaf, so it is safe to resolve during bootstrap. This is what lets
		// upstream `lib/repl.js` load: it compiles and runs in this realm, which
		// is all node's own REPL asks for.
		contextify: () => contextify,
		// Destructured at load by internal/http2/core.js (stream_pipe, only used by
		// the server-side respondWithFile) and internal/js_stream_socket.js
		// (js_stream). The client path never constructs either — the patched core.js
		// removes the JSStreamSocket wrap — so these only need to not throw at import.
		stream_pipe: () => ({
			StreamPipe: class StreamPipe {
				constructor() {
					throw new Error("StreamPipe is not available in this runtime");
				}
			},
		}),
		js_stream: () => ({
			JSStream: class JSStream {
				constructor() {
					throw new Error("JSStream is not available in this runtime");
				}
			},
		}),
	};
}

function internalBinding(name: string): any {
	lookup ??= buildLookup();
	const resolve = lookup[name];
	if (resolve === undefined) {
		throw new Error(
			`internalBinding('${name}') is not implemented in this runtime`
		);
	}
	const binding = resolve();
	if (binding === undefined) {
		throw new Error(
			`internalBinding('${name}') resolved to undefined — its module is ` +
				`probably still initializing (import cycle)`
		);
	}
	return binding;
}

export default internalBinding;
