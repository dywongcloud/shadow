import tls from '../../node/tls';

export const TLSSocket = tls.TLSSocket;
export const Server = tls.Server;
export const SecureContext = tls.SecureContext;
export const createServer = tls.createServer;
export const createSecureContext = tls.createSecureContext;
export const connect = tls.connect;
export const checkServerIdentity = tls.checkServerIdentity;
export const getCiphers = tls.getCiphers;
export const rootCertificates = tls.rootCertificates;
export const DEFAULT_ECDH_CURVE = tls.DEFAULT_ECDH_CURVE;
export const DEFAULT_MAX_VERSION = tls.DEFAULT_MAX_VERSION;
export const DEFAULT_MIN_VERSION = tls.DEFAULT_MIN_VERSION;
export const DEFAULT_CIPHERS = tls.DEFAULT_CIPHERS;

export default tls;
