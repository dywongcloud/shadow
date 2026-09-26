// Minimal `internal/url` override. Upstream is ~1700 lines built around the
// C++ URL parser and pulls in path/buffer/blob/crypto bindings; we expose
// the names that upstream `lib/url.js` (and other audited node-core
// consumers) actually destructure, backed by globalThis.URL.

import tr46 from 'tr46';

const URL = globalThis.URL;
const URLSearchParams = globalThis.URLSearchParams;

function isURL(value) {
  return value instanceof URL;
}

function fileURLToPath(url) {
  const u = url instanceof URL ? url : new URL(url);
  if (u.protocol !== 'file:') {
    throw new TypeError(`The URL must be of scheme file, received ${u.protocol}`);
  }
  return decodeURIComponent(u.pathname);
}

function fileURLToPathBuffer(url) {
  // We don't have node:buffer-backed paths here; return a string and let the
  // caller coerce. Callers in our audited tree expect a Buffer; if one ever
  // shows up we'll surface that here.
  return fileURLToPath(url);
}

function pathToFileURL(p) {
  return new URL('file://' + (p.startsWith('/') ? p : '/' + p));
}

function toPathIfFileURL(input) {
  if (!isURL(input)) return input;
  return fileURLToPath(input);
}

function URLParse(input, base) {
  return URL.parse(input, base);
}

function domainToASCII(domain) {
  if (arguments.length < 1) throw new TypeError('domain is required');
  return tr46.toASCII(String(domain)) ?? '';
}

function domainToUnicode(domain) {
  if (arguments.length < 1) throw new TypeError('domain is required');
  return tr46.toUnicode(String(domain)).domain;
}

function urlToHttpOptions(url) {
  const { hostname, pathname, port, username, password, search } = url;
  const options = {
    __proto__: null,
    ...url,
    protocol: url.protocol,
    hostname: hostname && hostname[0] === '[' ? hostname.slice(1, -1) : hostname,
    hash: url.hash,
    search: search,
    pathname: pathname,
    path: `${pathname || ''}${search || ''}`,
    href: url.href,
  };
  if (port !== '') {
    options.port = Number(port);
  }
  if (username || password) {
    options.auth = `${decodeURIComponent(username)}:${decodeURIComponent(password)}`;
  }
  return options;
}

const unsafeProtocol = new Set([
  'javascript',
  'javascript:',
]);
const hostlessProtocol = new Set([
  'javascript',
  'javascript:',
]);
const slashedProtocol = new Set([
  'http',
  'http:',
  'https',
  'https:',
  'ftp',
  'ftp:',
  'gopher',
  'gopher:',
  'file',
  'file:',
  'ws',
  'ws:',
  'wss',
  'wss:',
]);

// Returns the serialized origin of a URL (string or URL). Used by
// internal/http2 initOriginSet.
function getURLOrigin(url) {
  return (typeof url === 'string' ? new URL(url) : url).origin;
}

export {
  URL,
  URLSearchParams,
  URLParse,
  isURL,
  getURLOrigin,
  fileURLToPath,
  fileURLToPathBuffer,
  pathToFileURL,
  toPathIfFileURL,
  domainToASCII,
  domainToUnicode,
  urlToHttpOptions,
  unsafeProtocol,
  hostlessProtocol,
  slashedProtocol,
};

export default {
  URL,
  URLSearchParams,
  URLParse,
  isURL,
  getURLOrigin,
  fileURLToPath,
  fileURLToPathBuffer,
  pathToFileURL,
  toPathIfFileURL,
  domainToASCII,
  domainToUnicode,
  urlToHttpOptions,
  unsafeProtocol,
  hostlessProtocol,
  slashedProtocol,
};
