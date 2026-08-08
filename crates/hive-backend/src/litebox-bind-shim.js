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
const GUEST_IP = process.env.LITEBOX_GUEST_IP;
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
