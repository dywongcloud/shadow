// `internalBinding('url_pattern')` — exposes the WHATWG URLPattern constructor.
// Modern workers ship URLPattern on globalThis; if a runtime doesn't, the
// export is `undefined` and `new URLPattern(...)` throws at call time, which
// matches what upstream consumers expect for an unsupported feature.

export default {
	URLPattern: (globalThis as any).URLPattern,
};
