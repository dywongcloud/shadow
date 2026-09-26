// `internalBinding('trace_events')` — the C++ tracing bridge. This runtime has
// no V8 trace-event infrastructure, so tracing is a no-op: `trace(...)` does
// nothing and every category reports disabled. Upstream `internal/console/
// constructor.js` destructures `trace` at load, and the ported console timer
// helpers in `internal/util/debuglog.js` call it; both are satisfied by no-ops.

export default {
	// (eventType, category, name, id, ...args) — emit a trace event. No-op here.
	trace(): void {},
	// Upstream returns a Uint8Array whose byte 0 is non-zero when the category is
	// enabled. A zero byte means "disabled", which is what we always want.
	getCategoryEnabledBuffer(_category: string): Uint8Array {
		return new Uint8Array(1);
	},
	getEnabledCategories(): string | undefined {
		return undefined;
	},
};
