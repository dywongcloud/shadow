const optionValues = Object.freeze({
	'--abort-on-uncaught-exception': false,
	'--insecure-http-parser': false,
	'--max-http-header-size': 16 * 1024,
	'--use-env-proxy': false,
	// Select upstream's AsyncContextFrame AsyncLocalStorage implementation
	// (lib/internal/async_local_storage/async_context_frame.js) over the legacy
	// async_hooks path. The frame's cross-continuation propagation — which V8
	// normally provides via getContinuationPreservedEmbedderData — is supplied
	// here by the async_context_frame binding holder plus the await-transform and
	// microtask patches. Matches upstream's default (node_options.h: true).
	'--async-context-frame': true,
});

export function getOptionValue(name) {
	return optionValues[name];
}

export function getCLIOptionsInfo() {
	return {};
}

export function getOptionsAsFlagsFromBinding() {
	return [];
}

export function getAllowUnauthorized() {
	return false;
}

export function getEmbedderOptions() {
	return {};
}

export function generateConfigJsonSchema() {
	return {};
}

export function refreshOptions() {}

export default {
	getCLIOptionsInfo,
	getOptionValue,
	getOptionsAsFlagsFromBinding,
	getAllowUnauthorized,
	getEmbedderOptions,
	generateConfigJsonSchema,
	refreshOptions,
};
