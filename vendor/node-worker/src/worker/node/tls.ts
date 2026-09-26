// @ts-nocheck — node:tls's type surface is enormous and TLSSocket's override of
// net.Socket#connect fights it; this is a hand-written shim, like net/index.ts.
//
// TLS is terminated inside epoxy (rustls) and tunneled over Wisp end-to-end, so
// a TLSSocket is just a net.Socket whose underlying byte streams come from
// `client.connectTls()` instead of `client.connect()`. The handshake is already
// complete by the time connectTls() resolves, so the negotiated protocol/cipher
// and the peer certificate chain are available immediately.
//
// Not supported (epoxy can only establish TLS at connect time, against the
// bundled webpki roots): wrapping an existing socket via `new TLSSocket(sock)`,
// TLS servers (`tls.createServer`), client certificates, custom CA / a custom
// `secureContext`, disabling verification (`rejectUnauthorized: false`), and a
// `servername` that differs from `host` (rustls derives SNI from the host).
import nodeNet from "./net";
import { Socket } from "./net/socket";
import nodeBuffer from "./buffer";
import nodeCrypto from "./crypto";
import { getClient } from "../epoxy";

let Buffer = nodeBuffer.Buffer;

// rustls names cipher suites with its own variant spelling (e.g.
// `TLS13_AES_128_GCM_SHA256`). Node's getCipher().name historically returns the
// OpenSSL short name, while standardName uses the IANA form. This table covers
// the suites rustls negotiates (TLS 1.3 + the ECDHE/AEAD TLS 1.2 set).
const OPENSSL_CIPHER_NAMES = {
	TLS13_AES_128_GCM_SHA256: "TLS_AES_128_GCM_SHA256",
	TLS13_AES_256_GCM_SHA384: "TLS_AES_256_GCM_SHA384",
	TLS13_CHACHA20_POLY1305_SHA256: "TLS_CHACHA20_POLY1305_SHA256",
	TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256: "ECDHE-ECDSA-AES128-GCM-SHA256",
	TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384: "ECDHE-ECDSA-AES256-GCM-SHA384",
	TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256: "ECDHE-RSA-AES128-GCM-SHA256",
	TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384: "ECDHE-RSA-AES256-GCM-SHA384",
	TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256: "ECDHE-ECDSA-CHACHA20-POLY1305",
	TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256: "ECDHE-RSA-CHACHA20-POLY1305",
};

function standardCipherName(name) {
	if (name.startsWith("TLS13_")) return "TLS_" + name.slice("TLS13_".length);
	return name;
}

// epoxy reports the TLS version with rustls's spelling (e.g. "TLSv1_3"); Node's
// getProtocol() / getCipher().version use "TLSv1.3" / "TLSv1.2".
function normalizeProtocol(version) {
	if (!version) return null;
	if (version === "TLSv1_3") return "TLSv1.3";
	if (version === "TLSv1_2") return "TLSv1.2";
	return version.replace("_", ".");
}

// node accepts ALPNProtocols as string[] | Buffer[] | Buffer | Uint8Array
// (the latter two being length-prefixed wire format). epoxy wants a plain
// string[], so normalize down to that.
function normalizeALPN(protocols) {
	if (!protocols) return undefined;
	if (typeof protocols === "string") return [protocols];
	if (Array.isArray(protocols)) {
		let out = protocols.map((p) =>
			typeof p === "string" ? p : Buffer.from(p).toString("latin1")
		);
		return out.length ? out : undefined;
	}
	let buf = Buffer.from(protocols);
	let out = [];
	for (let i = 0; i < buf.length; ) {
		let len = buf[i++];
		if (!len || i + len > buf.length) break;
		out.push(buf.toString("latin1", i, i + len));
		i += len;
	}
	return out.length ? out : undefined;
}

// The libcrypto X509 binding prints subject/issuer as newline-separated
// `SN=value` lines (short names, no alignment); parse that into the object Node's
// getPeerCertificate() exposes. Repeated keys collapse into an array.
function parseX509Name(str) {
	let out = {};
	if (!str) return out;
	for (let raw of str.split("\n")) {
		let line = raw.trim();
		if (!line) continue;
		let eq = line.indexOf("=");
		if (eq < 0) continue;
		let key = line.slice(0, eq).trim();
		let val = line.slice(eq + 1).trim();
		let existing = out[key];
		if (existing === undefined) out[key] = val;
		else if (Array.isArray(existing)) existing.push(val);
		else out[key] = [existing, val];
	}
	return out;
}

// infoAccess prints as `Method - Kind:value` lines; values can contain ':'
// (URIs), so split on the first one only. Tolerant of unexpected formats.
function parseInfoAccess(str) {
	if (!str) return undefined;
	let out = {};
	for (let raw of str.split("\n")) {
		let line = raw.trim();
		if (!line) continue;
		let colon = line.indexOf(":");
		if (colon < 0) continue;
		let key = line.slice(0, colon).trim();
		let val = line.slice(colon + 1).trim();
		(out[key] ??= []).push(val);
	}
	return Object.keys(out).length ? out : undefined;
}

export class TLSSocket extends Socket {
	encrypted = true;
	authorized = false;
	authorizationError = null;
	alpnProtocol = false;
	servername = null;

	#protocol = null;
	#cipherSuite = null;
	#peerCertificates = [];
	#requestedALPN;
	#rejectUnauthorized = true;

	constructor(socket, options = {}) {
		super(options);
		if (socket) {
			throw new Error(
				"new tls.TLSSocket(socket): wrapping an existing socket is not supported in this runtime"
			);
		}
	}

	#applyMetadata(stream) {
		this.alpnProtocol = stream.negotiatedProtocol || false;
		this.#protocol = normalizeProtocol(stream.protocolVersion);
		this.#cipherSuite = stream.cipherSuite;
		this.#peerCertificates = stream.peerCertificates ?? [];
		// rustls verified the chain against the bundled webpki roots during the
		// handshake; verification failure would have rejected connectTls() before
		// we ever reach this point, so a live connection is always authorized.
		this.authorized = true;
	}

	connect(options = {}, ...rest) {
		let cb =
			typeof rest[rest.length - 1] === "function"
				? rest[rest.length - 1]
				: undefined;

		// (port[, host][, options][, cb]) form
		if (typeof options !== "object" || options === null) {
			let port = options;
			let host = typeof rest[0] === "string" ? rest[0] : undefined;
			let opts = rest.find((a) => typeof a === "object" && a !== null) ?? {};
			options = { ...opts, port, host: host ?? opts.host };
		}

		let host = options.host ?? "localhost";
		let port = Number(options.port);
		if (!Number.isInteger(port) || port < 0 || port > 65535) {
			throw new RangeError("port must be an integer between 0 and 65535");
		}

		this.#requestedALPN = normalizeALPN(options.ALPNProtocols);
		this.#rejectUnauthorized = options.rejectUnauthorized !== false;
		this.servername =
			typeof options.servername === "string"
				? options.servername
				: nodeNet.isIP(host)
					? null
					: host;

		if (cb) this.once("secureConnect", cb);

		let alpn = this.#requestedALPN;
		this._beginConnect(
			host,
			port,
			async () => {
				let client = await getClient();
				let stream = await client.connectTls(
					host,
					port,
					alpn ? { alpn } : {}
				);
				this.#applyMetadata(stream);
				return { read: stream.read, write: stream.write };
			},
			() => this.emit("secureConnect")
		);

		return this;
	}

	getProtocol() {
		return this.#protocol;
	}

	getCipher() {
		if (!this.#cipherSuite) return {};
		let standardName = standardCipherName(this.#cipherSuite);
		return {
			name: OPENSSL_CIPHER_NAMES[this.#cipherSuite] ?? standardName,
			standardName,
			version: this.#protocol ?? "TLSv1.3",
		};
	}

	getALPNProtocol() {
		return this.alpnProtocol;
	}

	getPeerX509Certificate() {
		let der = this.#peerCertificates[0];
		if (!der) return undefined;
		return new nodeCrypto.X509Certificate(Buffer.from(der));
	}

	#legacyCert(der) {
		let x509 = new nodeCrypto.X509Certificate(Buffer.from(der));
		let cert = {
			subject: parseX509Name(x509.subject),
			issuer: parseX509Name(x509.issuer),
			subjectaltname: x509.subjectAltName ?? undefined,
			valid_from: x509.validFrom,
			valid_to: x509.validTo,
			fingerprint: x509.fingerprint,
			fingerprint256: x509.fingerprint256,
			fingerprint512: x509.fingerprint512,
			serialNumber: x509.serialNumber,
			raw: Buffer.from(der),
		};
		let info = parseInfoAccess(x509.infoAccess);
		if (info) cert.infoAccess = info;
		return cert;
	}

	getPeerCertificate(detailed) {
		if (!this.#peerCertificates.length) return {};
		if (!detailed) return this.#legacyCert(this.#peerCertificates[0]);

		// Chain is end-entity first; each cert's issuer is the next one, and the
		// self-issued root points at itself (matching Node, which avoids a null
		// terminator so callers can walk issuerCertificate without a guard).
		let chain = this.#peerCertificates.map((der) => this.#legacyCert(der));
		for (let i = 0; i < chain.length; i++) {
			chain[i].issuerCertificate = chain[i + 1] ?? chain[i];
		}
		return chain[0];
	}

	// --- Best-effort / no-op surface for API compatibility ---
	getSession() {
		return undefined;
	}
	getTLSTicket() {
		return undefined;
	}
	isSessionReused() {
		return false;
	}
	getEphemeralKeyInfo() {
		return {};
	}
	getFinished() {
		return undefined;
	}
	getPeerFinished() {
		return undefined;
	}
	getSharedSigalgs() {
		return [];
	}
	getCertificate() {
		return null;
	}
	exportKeyingMaterial() {
		throw new Error(
			"tls: exportKeyingMaterial is not supported in this runtime"
		);
	}
	setMaxSendFragment() {
		return true;
	}
	enableTrace() {}
	disableRenegotiation() {}
	renegotiate(_options, callback) {
		if (typeof callback === "function") {
			queueMicrotask(() =>
				callback(new Error("tls: renegotiation is not supported in this runtime"))
			);
		}
		return false;
	}
}

// A SecureContext in real Node wraps a native handle holding ca/cert/key/etc.
// epoxy doesn't accept any of that yet, so this just retains the options so
// callers (e.g. https.Agent) can pass one around without crashing.
class SecureContext {
	constructor(options = {}) {
		this.context = { ...options };
	}
}

function createSecureContext(options = {}) {
	return new SecureContext(options);
}

function connect(...args) {
	let cb = typeof args[args.length - 1] === "function" ? args.pop() : undefined;

	let options;
	if (typeof args[0] === "object" && args[0] !== null) {
		options = { ...args[0] };
	} else {
		options = {};
		if (args[0] !== undefined) options.port = args[0];
		if (typeof args[1] === "string") options.host = args[1];
		let extra = args.find(
			(a, i) => i > 0 && typeof a === "object" && a !== null
		);
		if (extra) options = { ...extra, ...options };
	}

	let socket = new TLSSocket(null, options);
	socket.connect(options, cb);
	return socket;
}

// Standard RFC 6125-ish identity check. epoxy/rustls already verifies the SNI
// hostname against the presented certificate during the handshake (so a live
// TLSSocket has already passed this), but some libraries call it explicitly.
function checkServerIdentity(hostname, cert) {
	let dnsNames = [];
	let ips = [];
	if (cert && cert.subjectaltname) {
		for (let entry of String(cert.subjectaltname).split(",")) {
			entry = entry.trim();
			if (entry.startsWith("DNS:")) dnsNames.push(entry.slice(4));
			else if (entry.startsWith("IP Address:")) ips.push(entry.slice(11));
			else if (entry.startsWith("IP:")) ips.push(entry.slice(3));
		}
	}
	// CN fallback only when there are no SAN dNSName entries (per the spec).
	if (!dnsNames.length && cert && cert.subject) {
		let cn = cert.subject.CN;
		if (Array.isArray(cn)) cn = cn[cn.length - 1];
		if (cn) dnsNames.push(cn);
	}

	let host = (hostname || "").toLowerCase();
	let matchesDns = (pattern) => {
		pattern = pattern.toLowerCase();
		if (pattern === host) return true;
		// leftmost-label wildcard, e.g. *.example.com
		if (pattern.startsWith("*.")) {
			let suffix = pattern.slice(1); // ".example.com"
			let dot = host.indexOf(".");
			return dot > 0 && host.slice(dot) === suffix;
		}
		return false;
	};

	let ok = nodeNet.isIP(host)
		? ips.some((ip) => ip.toLowerCase() === host)
		: dnsNames.some(matchesDns);
	if (ok) return undefined;

	let names = [
		...dnsNames.map((d) => `DNS:${d}`),
		...ips.map((i) => `IP:${i}`),
	].join(", ");
	let err = new Error(
		`Hostname/IP does not match certificate's altnames: Host: ${hostname}. is not in the cert's altnames: ${names}`
	);
	err.code = "ERR_TLS_CERT_ALTNAME_INVALID";
	err.reason = "no matching subject altname";
	err.host = hostname;
	err.cert = cert;
	return err;
}

// A small lowercase list of the suites epoxy can negotiate, in Node's getCiphers
// format. Not exhaustive, but enough for code that probes availability.
function getCiphers() {
	return [
		"tls_aes_128_gcm_sha256",
		"tls_aes_256_gcm_sha384",
		"tls_chacha20_poly1305_sha256",
		"ecdhe-ecdsa-aes128-gcm-sha256",
		"ecdhe-ecdsa-aes256-gcm-sha384",
		"ecdhe-rsa-aes128-gcm-sha256",
		"ecdhe-rsa-aes256-gcm-sha384",
		"ecdhe-ecdsa-chacha20-poly1305",
		"ecdhe-rsa-chacha20-poly1305",
	];
}

function unsupported(name) {
	return () => {
		throw new Error(`node:tls.${name} is not supported in this runtime`);
	};
}

class Server {
	constructor() {
		throw new Error("node:tls server is not supported in this runtime");
	}
}

const createServer = unsupported("createServer");
const rootCertificates = [];
const DEFAULT_ECDH_CURVE = "auto";
const DEFAULT_MAX_VERSION = "TLSv1.3";
const DEFAULT_MIN_VERSION = "TLSv1.2";
const DEFAULT_CIPHERS = "";

const tls = {
	TLSSocket,
	Server,
	SecureContext,
	createServer,
	createSecureContext,
	connect,
	checkServerIdentity,
	getCiphers,
	rootCertificates,
	DEFAULT_ECDH_CURVE,
	DEFAULT_MAX_VERSION,
	DEFAULT_MIN_VERSION,
	DEFAULT_CIPHERS,
};

export default tls as unknown as typeof import("node:tls");
