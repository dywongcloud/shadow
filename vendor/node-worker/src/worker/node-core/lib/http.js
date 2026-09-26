import httpAgent from '_http_agent';
import httpClient from '_http_client';
import httpCommon from '_http_common';
import httpIncoming from '_http_incoming';
import httpOutgoing from '_http_outgoing';
import httpServer from '_http_server';
import internalHttp from 'internal/http';
import internalValidators from 'internal/validators';
import internalErrors from 'internal/errors';
import internalOptions from 'internal/options';

const {
  validateInteger,
  validateObject,
} = internalValidators;
const { ERR_PROXY_INVALID_CONFIG } = internalErrors.codes;
const { ClientRequest } = httpClient;
const { methods, parsers } = httpCommon;
const { IncomingMessage } = httpIncoming;
const {
  validateHeaderName,
  validateHeaderValue,
  OutgoingMessage,
} = httpOutgoing;
const {
  _connectionListener,
  STATUS_CODES,
  Server,
  ServerResponse,
} = httpServer;
const {
  parseProxyUrl,
  getGlobalAgent,
} = internalHttp;

let maxHeaderSize;
let globalAgent = httpAgent.globalAgent;

function createServer(opts, requestListener) {
  return new Server(opts, requestListener);
}

function request(url, options, cb) {
  return new ClientRequest(url, options, cb);
}

function get(url, options, cb) {
  const req = request(url, options, cb);
  req.end();
  return req;
}

function setGlobalProxyFromEnv(env = process.env) {
  validateObject(env, 'proxyEnv');
  const httpProxy = parseProxyUrl(env, 'http:');
  const httpsProxy = parseProxyUrl(env, 'https:');

  if (httpsProxy) {
    throw new ERR_PROXY_INVALID_CONFIG('HTTPS proxy is not supported in this runtime');
  }
  if (!httpProxy) {
    return () => {};
  }

  const previousGlobalAgent = globalAgent;
  globalAgent = getGlobalAgent(env, httpAgent.Agent);

  return function restore() {
    globalAgent = previousGlobalAgent;
  };
}

function setMaxIdleHTTPParsers(max) {
  validateInteger(max, 'max', 1);
  parsers.max = max;
}

export {
  _connectionListener,
  IncomingMessage,
  OutgoingMessage,
  STATUS_CODES,
  ClientRequest,
  Server,
  ServerResponse,
  createServer,
  get,
  request,
  setGlobalProxyFromEnv,
  setMaxIdleHTTPParsers,
  validateHeaderName,
  validateHeaderValue,
};

export const Agent = httpAgent.Agent;
export const METHODS = Array.from(methods).sort();

export { globalAgent };

export default {
  _connectionListener,
  METHODS,
  STATUS_CODES,
  Agent,
  ClientRequest,
  IncomingMessage,
  OutgoingMessage,
  Server,
  ServerResponse,
  createServer,
  validateHeaderName,
  validateHeaderValue,
  get,
  request,
  setMaxIdleHTTPParsers,
  setGlobalProxyFromEnv,
  get maxHeaderSize() {
    if (maxHeaderSize === undefined) {
      maxHeaderSize = internalOptions.getOptionValue('--max-http-header-size');
    }

    return maxHeaderSize;
  },
  get globalAgent() {
    return globalAgent;
  },
  set globalAgent(value) {
    globalAgent = value;
  },
};
