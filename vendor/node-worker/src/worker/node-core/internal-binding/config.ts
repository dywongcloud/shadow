// `internalBinding('config')` — compile-time feature flags. This runtime has no
// ICU (`internal/util/inspect.js` falls back to its pure-JS string-width table
// when hasIntl is false) and no V8 inspector.

export default {
	hasIntl: false,
	hasInspector: false,
	hasOpenSSL: false,
	hasTracing: false,
	fipsMode: false,
	bits: 64,
};
