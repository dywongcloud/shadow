'use strict';
// Litebox's guest network stack (see hive_backend::litebox's module doc,
// "Networking" section) cannot bridge host<->guest LOOPBACK over a TUN
// device -- an app that explicitly hardcodes `.listen(port, '127.0.0.1')`
// (or, belt-and-suspenders, an ordinary wildcard bind, in case the litebox
// wildcard-bind patch is ever missing) needs its bind address rewritten to
// this cell's real, directly-routable guest IP. Preloaded via
// NODE_OPTIONS="--require <this file>" -- zero tenant code changes needed.
//
// Patches net.Server.prototype._listen2 (Node's internal alias for
// setupListenHandle), NOT the public .listen() -- by this point Node has
// already normalized every real .listen() overload (options object,
// (port,host), (port,cb), DNS-resolved hostnames, cluster IPC) down to one
// uniform (address, port, addressType, backlog, fd, flags) signature, so
// there is no overload-shape parsing left to get wrong here. This is a
// deliberately preserved monkeypatch seam -- Node's own source comment on
// the method says as much -- stable across Node v10-v24, and the same
// technique New Relic's Node agent has run in production since ~2012.
// http/https/http2/ws all inherit net.Server.prototype unmodified, so this
// covers them automatically; NODE_OPTIONS is inherited by `cluster` workers
// automatically too.
const net = require('net');
const WORKDIR = process.env.HIVE_RUNTIME_WORKDIR;
if (WORKDIR) {
  process.chdir(WORKDIR);
}
// Litebox's shim does not implement the native realpath fast path libuv uses,
// so `fs.realpathSync.native` / `fs.realpath.native` fail ENOENT on paths that
// demonstrably exist, while the pure-JS implementations (lstat + readlink)
// work. Next.js's `getProjectDir` calls the NATIVE variant, which turned every
// `next start` in this guest into "Invalid project directory provided, no such
// directory: <workdir>" -- witnessed live on nodes-wtf. Route the native
// variants onto the JS implementations; in this guest the artifact tree is
// symlink-free by construction, so the two are semantically identical.
const fs = require('fs');
if (typeof fs.realpathSync === 'function') {
  fs.realpathSync.native = fs.realpathSync.bind(fs);
}
if (typeof fs.realpath === 'function') {
  fs.realpath.native = fs.realpath.bind(fs);
}
const GUEST_IP = process.env.LITEBOX_GUEST_IP;
// Litebox does not implement `uv_interface_addresses`, so
// `os.networkInterfaces()` throws ERR_SYSTEM_ERROR (errno 97) — which Next.js
// calls unconditionally right after `listening` fires (get-network-host.js) to
// print its "Network:" banner, turning a successfully LISTENING server into an
// unhandled-rejection crash. Witnessed live on nodes-wtf. Answer with this
// cell's real TUN address when the native call fails; the address is genuinely
// this guest's routable interface, so callers get truth, not a stub.
const os = require('os');
const origNetworkInterfaces = os.networkInterfaces.bind(os);
os.networkInterfaces = function () {
  try {
    return origNetworkInterfaces();
  } catch (_) {
    if (GUEST_IP) {
      return {
        lb0: [{
          address: GUEST_IP,
          netmask: '255.255.255.252',
          family: 'IPv4',
          mac: '00:00:00:00:00:00',
          internal: false,
          cidr: GUEST_IP + '/30',
        }],
      };
    }
    return {};
  }
};
const NEEDS_REWRITE = new Set(['0.0.0.0', '::', '127.0.0.1', '::1', 'localhost']);
if (GUEST_IP) {
  const orig = net.Server.prototype._listen2;
  net.Server.prototype._listen2 = function (address, port, addressType, backlog, fd, flags) {
    const isPipe = addressType === -1 && port === -1; // unix socket / named pipe
    const isFd = typeof fd === 'number' && fd >= 0; // pre-opened fd
    if (!isPipe && !isFd && (!address || NEEDS_REWRITE.has(address))) {
      address = GUEST_IP;
      addressType = 4;
    }
    return orig.call(this, address, port, addressType, backlog, fd, flags);
  };
}
