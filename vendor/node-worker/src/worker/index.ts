// Must stay first: it initializes primordials before any node-core module that
// reads them is evaluated. Note that this module must not reference the
// injected globals (`process`, `internalBinding`, `primordials`) even once —
// rollup's inject plugin would prepend an import for them *above* this line,
// pulling the node subgraph in ahead of the bootstrap.
import "./early-import";

import type { ControlCall } from "../wire/control";
import type { PuterFsEvent } from "../wire/events";
import { KIND_CONTROL } from "../wire/kinds";
import { makeDispatcher } from "../wire/router";

import { setEpoxyBase } from "./epoxy";
// Applied before a run's module is required; see the `ctl.execute` handler.
import { initThread } from "./node/worker_threads";
import { PUTER_TOKEN, setNet, setPuterCWD, setPuterToken } from "./state";
import {
	apiStatsEnabled,
	fetchUserInfo,
	reportRequestStats,
	resetRequestStats,
	setAnonymousUser,
} from "./puter";
import { require } from "./module/cjs";
import { esmImport } from "./module/esm";
import { emitLocalFsEvent } from "./fsevents";
import {
	invalidateResolved,
	invalidateResolvedSubtree,
} from "./module/resolve";
import { setArgv, setEnv, takeExitCode } from "./node/process";
import { ProcessExit } from "./exit";
import { flushConsole, initConsole, setIsTTY, setTTYSize } from "./console";
import { bootstrap, wire } from "./wire";
import { drain, installPlatformRefs, setKeepaliveEnabled } from "./keepalive";
// Imported for its side effect: the module registers the `chan` dispatcher on the
// router as it evaluates, the same way ./fsevents.ts registers `ev`. A leaf module — a
// Map and two functions — so a direct edge from the entry is safe.
import "./channels";
// A leaf module (no `process`, no primordials), so a direct edge from the entry is safe here
// where an edge into the fs barrel would not be.
import {
	applyMountSnapshot,
	getHopStats,
	initTransport,
	onReplyMeta,
	resetHopStats,
} from "./node/fs/transport";

// Control is one dispatcher among the kinds now, registered on the same router that
// answers filesystem and process messages. It used to be `setMessageHandler` — a second
// envelope with its own inflight map, its own error shape and its own way of telling a
// reply from a request.
wire.router.register(
	KIND_CONTROL,
	makeDispatcher<ControlCall>(KIND_CONTROL, async (m, _parts, attachments) => {
		if (m.op === "ctl.init") {
			setPuterToken(m.puter);
			if (m.net) setNet(m.net);
			setEpoxyBase(m.epoxyBase);
			setPuterCWD(m.cwd);
			setIsTTY(m.isTTY);
			if (m.size) setTTYSize(m.size);
			initConsole({ isTTY: m.isTTY });
			setKeepaliveEnabled(!!m.keepalive);
			// Before anything can compile wasm — a package's bundler is the one that matters,
			// and epoxy's own init (now lazy, see ./epoxy) compiles through these too.
			installPlatformRefs();
			// `whoami` is an authenticated call, so an anonymous run has no user to fetch
			// and takes a placeholder one instead — see `setAnonymousUser`.
			if (PUTER_TOKEN) await fetchUserInfo();
			else setAnonymousUser();
			// Last, and reported back: this probes the synchronous filesystem transport with one
			// round trip, so a service worker that is not actually intercepting becomes a startup
			// state the host can act on instead of a hang at the first `readFileSync`.
			// Watch events and resolver-cache invalidations ride the reply of the call that caused
			// them, and this is where they are applied.
			//
			// Both were direct function calls before the filesystem moved: providers called
			// `emitLocalFsEvent` themselves, and the memory-mount API called into the resolver's
			// caches. Losing either fails *silently* — a dev server stops noticing that files
			// changed, and a file the host wrote is answered as "missing" forever, which only shows
			// up on a second run. Registered here rather than imported by the transport so that
			// module keeps no edge into the fsevents or resolver subgraphs.
			onReplyMeta((meta) => {
				if (meta.events) {
					for (let event of meta.events)
						emitLocalFsEvent(event as PuterFsEvent);
				}
				if (meta.invalidate) {
					for (let path of meta.invalidate.paths ?? [])
						invalidateResolved(path);
					for (let root of meta.invalidate.subtrees ?? []) {
						invalidateResolvedSubtree(root);
					}
				}
			});

			let capabilities = initTransport(m.vfs);
			return { value: { capabilities } };
		}
		if (m.op === "ctl.cwd") {
			setPuterCWD(m.cwd);
			return;
		}
		if (m.op === "ctl.execute") {
			setArgv(m.argv ?? ["node", m.target]);
			if (m.env) setEnv(m.env);
			/*
			 * Before the module, not after: `require("worker_threads").parentPort` is read at
			 * module scope, so by the time the first line runs the port has to be a port. The
			 * page makes that possible by awaiting `chan.open` before sending this call, and
			 * `workerData` rides as a structured-cloned attachment because JSON would flatten a
			 * Map or a typed array into something else.
			 */
			initThread(
				m.thread && {
					...m.thread,
					workerData: m.thread.workerData ?? attachments[0],
				}
			);

			// Every puter API call is a round trip, and on the resolver's path a
			// *blocking* one, so the per-endpoint call count is the number worth
			// watching when tuning resolution or readdir. Opt-in via
			// NODE_WORKER_API_STATS; `reportRequestStats` decides where it goes.
			//
			// Read *after* `setEnv`, which replaces `process.env` wholesale for this run — so
			// checking first meant the flag could only ever be seen if it had been set by some
			// earlier run, and never when passed on the `execute` that wanted it.
			let stats = apiStatsEnabled();
			if (stats) {
				resetRequestStats();
				resetHopStats();
			}

			let exitCode: number;
			try {
				if (m.module === "esm") await esmImport(m.target);
				else if (m.module === "cjs") await require(m.target);
				await drain();
				exitCode = takeExitCode();
			} catch (err) {
				// `process.exit` does not wait for the event loop, so this deliberately
				// skips the drain a normal return goes through. The page is terminating
				// us anyway; replying keeps the exit code correct for a caller that
				// chooses not to.
				if (!(err instanceof ProcessExit)) throw err;
				exitCode = err.code;
			}

			// Reported *before* the flush, not after. `reportRequestStats` prints to the
			// program's own stderr, and the flush below is the only thing that guarantees
			// output has crossed the boundary before the reply — which is what lets the host
			// tear this worker down. Reporting afterwards raced that teardown, so the stats
			// were routinely lost: a miserable way to lose a diagnostic whose whole job is to
			// be read.
			if (stats) reportRequestStats(getHopStats());

			// Everything the program printed has to be across the boundary before the reply,
			// because the reply is what lets the host tear this worker down.
			await flushConsole();

			return { value: { exitCode } };
		}
		if (m.op === "ctl.mounts") {
			applyMountSnapshot(m.mounts);
			return;
		}
		if (m.op === "ctl.setTty") {
			setIsTTY(m.isTTY);
			if (m.size) setTTYSize(m.size);
			return;
		}

		// The worker-to-page half of the kind — `ctl.hi`, `ctl.tty`, `ctl.exit` — is answered
		// by the page, never here.
		throw Object.assign(
			new Error(`control op ${(m as { op: string }).op} is not for the worker`),
			{ code: "ENOSYS" }
		);
	})
);

// Nothing is announced at startup any more. There used to be a `hi` message the page
// waited for before sending `init`, which is a handshake the platform already provides:
// a `postMessage` to a worker whose script has not finished evaluating is queued, not
// dropped. So the page posts the bootstrap immediately and the reply to `ctl.init` is the
// readiness signal — one round trip instead of two.
bootstrap();
