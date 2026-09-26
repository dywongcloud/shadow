import { Buffer } from 'buffer';
import { setUnrefTimeout } from 'internal/timers';
import { codes } from './errors.js';
import { isIPv4 } from './net.js';

export const kOutHeaders = Symbol('kOutHeaders');
export const kNeedDrain = Symbol('kNeedDrain');
export const kProxyConfig = Symbol('kProxyConfig');
export const kWaitForProxyTunnel = Symbol('kWaitForProxyTunnel');

let utcCache;
let traceEventId = 0;

function resetCache() {
	utcCache = undefined;
}

export function utcDate() {
	if (!utcCache) {
		const now = new Date();
		utcCache = now.toUTCString();
		setUnrefTimeout(resetCache, 1000 - now.getMilliseconds());
	}
	return utcCache;
}

export function getNextTraceEventId() {
	return ++traceEventId;
}

export function isTraceHTTPEnabled() {
	return false;
}

export function traceBegin() {}
export function traceEnd() {}

function ipToInt(ip) {
	return ip.split('.').reduce((result, part) => ((result << 8) + Number.parseInt(part, 10)) >>> 0, 0);
}

class ProxyConfig {
	constructor(proxyUrl, noProxyList) {
		let parsed;
		try {
			parsed = new URL(proxyUrl);
		} catch {
			throw new codes.ERR_PROXY_INVALID_CONFIG(`Invalid proxy URL: ${proxyUrl}`);
		}

		const { hostname, port, protocol, username, password } = parsed;
		this.href = proxyUrl;
		this.protocol = protocol;
		if (username || password) {
			const auth = `${decodeURIComponent(username)}:${decodeURIComponent(password)}`;
			this.auth = `Basic ${Buffer.from(auth).toString('base64')}`;
		}
		this.bypassList = noProxyList ? noProxyList.split(',').map((entry) => entry.trim().toLowerCase()) : [];
		this.proxyConnectionOptions = {
			host: hostname[0] === '[' ? hostname.slice(1, -1) : hostname,
			port: port ? Number(port) : (protocol === 'https:' ? 443 : 80),
		};
	}

	shouldUseProxy(hostname, port) {
		if (this.bypassList.length === 0) {
			return true;
		}

		const host = String(hostname).toLowerCase();
		const hostWithPort = port ? `${host}:${port}` : host;
		for (const entry of this.bypassList) {
			if (!entry) continue;
			if (entry === '*' || entry === host || entry === hostWithPort) return false;
			if (entry[0] === '.' && host.endsWith(entry.slice(1))) return false;
			if (entry.startsWith('*.') && host.endsWith(entry.slice(1))) return false;
			if (entry.includes('-') && isIPv4(host)) {
				const [startIp, endIp] = entry.split('-').map((value) => value.trim());
				if (isIPv4(startIp) && isIPv4(endIp)) {
					const hostInt = ipToInt(host);
					if (hostInt >= ipToInt(startIp) && hostInt <= ipToInt(endIp)) return false;
				}
			}
		}

		return true;
	}
}

export function parseProxyUrl(env, protocol) {
	const proxyUrl = protocol === 'https:'
		? (env?.https_proxy || env?.HTTPS_PROXY)
		: (env?.http_proxy || env?.HTTP_PROXY);
	if (!proxyUrl) {
		return null;
	}
	if (proxyUrl.includes('\r') || proxyUrl.includes('\n')) {
		throw new codes.ERR_PROXY_INVALID_CONFIG(`Invalid proxy URL: ${proxyUrl}`);
	}
	return proxyUrl;
}

export function parseProxyConfigFromEnv(env, protocol) {
	if (protocol !== 'http:' && protocol !== 'https:') {
		return null;
	}
	const proxyUrl = parseProxyUrl(env, protocol);
	if (proxyUrl === null) {
		return null;
	}
	if (!proxyUrl.startsWith('http://') && !proxyUrl.startsWith('https://')) {
		return null;
	}
	return new ProxyConfig(proxyUrl, env?.no_proxy || env?.NO_PROXY);
}

export function checkShouldUseProxy(proxyConfig, reqOptions) {
	if (!proxyConfig || reqOptions?.socketPath) {
		return false;
	}
	return proxyConfig.shouldUseProxy(reqOptions.host || 'localhost', reqOptions.port);
}

export function getGlobalAgent(proxyEnv, Agent) {
	return new Agent({
		keepAlive: true,
		scheduling: 'lifo',
		timeout: 5000,
		proxyEnv,
	});
}

export default {
	utcDate,
	kOutHeaders,
	kNeedDrain,
	isTraceHTTPEnabled,
	traceBegin,
	traceEnd,
	getNextTraceEventId,
	kProxyConfig,
	kWaitForProxyTunnel,
	parseProxyUrl,
	parseProxyConfigFromEnv,
	checkShouldUseProxy,
	getGlobalAgent,
};
