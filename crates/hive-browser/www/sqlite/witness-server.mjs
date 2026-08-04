// witness-server.mjs — static server for crates/hive-browser/www PLUS a
// same-origin /api/* proxy to the local hive-cloud admin port, so the
// witness page's fetches stay same-origin (the admin API has no CORS layer
// by design; production browser traffic reaches it through the platform's
// own hosts, not arbitrary page origins).
//
//   node witness-server.mjs <listen-port> <www-root> <api-upstream-url>

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const [port, root, upstream] = [Number(process.argv[2]), process.argv[3], process.argv[4]];
const MIME = {
  '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript',
  '.wasm': 'application/wasm', '.json': 'application/json', '.css': 'text/css',
  '.png': 'image/png', '.svg': 'image/svg+xml', '.ico': 'image/x-icon',
};

http.createServer((req, res) => {
  const url = new URL(req.url, 'http://localhost');
  if (url.pathname.startsWith('/api/')) {
    const target = new URL(url.pathname.slice(4) + url.search, upstream);
    const proxy = http.request(
      { hostname: target.hostname, port: target.port, path: target.pathname + target.search, method: req.method, headers: { ...req.headers, host: target.host } },
      (up) => {
        res.writeHead(up.statusCode, up.headers);
        up.pipe(res);
      });
    proxy.on('error', (err) => { res.writeHead(502); res.end(String(err)); });
    req.pipe(proxy);
    return;
  }
  const file = path.join(root, decodeURIComponent(url.pathname));
  if (!file.startsWith(root)) { res.writeHead(403); res.end(); return; }
  fs.readFile(file.endsWith('/') ? path.join(file, 'index.html') : file, (err, data) => {
    if (err) { res.writeHead(404); res.end('not found'); return; }
    res.writeHead(200, { 'content-type': MIME[path.extname(file)] ?? 'application/octet-stream' });
    res.end(data);
  });
}).listen(port, '127.0.0.1', () => console.log(`witness-server on :${port} (api -> ${upstream})`));
