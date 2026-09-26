// `internal/util/inspector` — there is no V8 inspector in a Worker.
//
// This is an override for weight as much as for correctness. Upstream it reaches
// `internal/process/execution` to run inspector-supplied code, and that pulls
// `internal/modules/run_main`, `internal/modules/typescript` and so
// `internal/deps/amaro/dist/index` — a single 3.3 MB TypeScript stripper. Bundling
// that took the worker build from five seconds to longer than anyone waited.
//
// `lib/repl.js:135` and `internal/repl/completion.js:41` want `sendInspectorCommand`,
// which is written for exactly this case: no inspector session, so take the other
// branch. Everything else here answers "there is no inspector" in whatever shape the
// caller expects.

function isUsingInspector() {
	return false;
}

/**
 * Upstream: open a session, hand it to `cb`, and fall back to `onError` when there
 * is no inspector to open one on. Only the fallback exists here, so the REPL does
 * the non-inspector thing — which for `createContext` is simply to skip announcing
 * the new context, and for completion to complete without the inspector's help.
 */
function sendInspectorCommand(_cb, onError) {
	return onError?.();
}

function getInspectPort() {
	return undefined;
}

function isInspectorMessage() {
	return false;
}

function installConsoleExtensions(commandLineApi) {
	return commandLineApi;
}

function wrapConsole() {}

export {
	getInspectPort,
	installConsoleExtensions,
	isInspectorMessage,
	isUsingInspector,
	sendInspectorCommand,
	wrapConsole,
};

export default {
	getInspectPort,
	installConsoleExtensions,
	isInspectorMessage,
	isUsingInspector,
	sendInspectorCommand,
	wrapConsole,
};
