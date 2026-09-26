import net from '../../node/net';

export const Socket = net.Socket;
export const Server = net.Server;
export const _normalizeArgs = net._normalizeArgs;
export const createServer = net.createServer;
export const connect = net.connect;
export const createConnection = net.createConnection;
export const isIP = net.isIP;
export const isIPv4 = net.isIPv4;
export const isIPv6 = net.isIPv6;
export const getDefaultAutoSelectFamily = net.getDefaultAutoSelectFamily;
export const setDefaultAutoSelectFamily = net.setDefaultAutoSelectFamily;
export const getDefaultAutoSelectFamilyAttemptTimeout = net.getDefaultAutoSelectFamilyAttemptTimeout;
export const setDefaultAutoSelectFamilyAttemptTimeout = net.setDefaultAutoSelectFamilyAttemptTimeout;

export default net;
