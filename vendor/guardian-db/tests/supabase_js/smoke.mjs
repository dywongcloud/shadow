// Optional supabase-js compatibility smoke test.
//
// This is intentionally NOT wired into `cargo test` — it needs Node + a network
// `npm install`, so it can never break Rust CI. Run it by hand:
//
//   cargo build --features supabase --bin guardian-supabase
//   cd tests/supabase_js && npm install && node smoke.mjs
//
// It spawns the real `guardian-supabase` binary, points the actual
// `@supabase/supabase-js` client at it, and exercises the Auth (GoTrue) and
// REST (PostgREST) paths. Override the binary/port with GUARDIAN_SUPABASE_BIN
// and GUARDIAN_SUPABASE_ADDR.

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { createClient } from '@supabase/supabase-js';

const here = dirname(fileURLToPath(import.meta.url));
const BIN =
  process.env.GUARDIAN_SUPABASE_BIN ||
  resolve(here, '../../target/debug/guardian-supabase');
const ADDR = process.env.GUARDIAN_SUPABASE_ADDR || '127.0.0.1:54329';
const SECRET = 'node-smoke-test-secret-0123456789abcdef';

const proc = spawn(BIN, ['--addr', ADDR, '--jwt-secret', SECRET], {
  stdio: ['ignore', 'pipe', 'pipe'],
});

let out = '';
const ready = new Promise((resolve, reject) => {
  const t = setTimeout(() => reject(new Error('server start timeout:\n' + out)), 15000);
  proc.stdout.on('data', (d) => {
    out += d;
    if (out.includes('listening on')) {
      clearTimeout(t);
      resolve();
    }
  });
  proc.stderr.on('data', (d) => (out += d));
  proc.on('exit', (c) => reject(new Error('server exited ' + c + '\n' + out)));
});

function fail(msg) {
  console.error('FAIL:', msg);
  proc.kill();
  process.exit(1);
}

try {
  await ready;
  const anon = out.match(/ANON_KEY\s*:\s*(\S+)/)[1];
  const supabase = createClient(`http://${ADDR}`, anon);

  // GoTrue: sign up through the real client.
  const email = `node-${Date.now()}@example.com`;
  const { data: su, error: e1 } = await supabase.auth.signUp({
    email,
    password: 'hunter2pass',
  });
  if (e1) fail('signUp error: ' + JSON.stringify(e1));
  if (su?.user?.email !== email) fail('signUp user mismatch: ' + JSON.stringify(su));
  console.log('  auth.signUp -> user', su.user.email);

  // GoTrue: sign in with password.
  const { data: si, error: e2 } = await supabase.auth.signInWithPassword({
    email,
    password: 'hunter2pass',
  });
  if (e2) fail('signIn error: ' + JSON.stringify(e2));
  if (!si?.session?.access_token) fail('no access_token: ' + JSON.stringify(si));
  console.log('  auth.signInWithPassword -> access_token present');

  // PostgREST: a missing table surfaces a PostgREST-shaped error (code 42P01).
  const { error: e3 } = await supabase.from('does_not_exist').select('*');
  if (!e3 || e3.code !== '42P01') fail('expected PostgREST 42P01, got ' + JSON.stringify(e3));
  console.log('  from().select() -> PostgREST error code', e3.code, '(expected)');

  console.log('SUPABASE-JS SMOKE: PASS');
  proc.kill();
  process.exit(0);
} catch (err) {
  fail(String(err));
}
