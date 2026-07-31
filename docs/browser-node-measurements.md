# Browser-node substrate measurements

Live in-browser benchmark backing the edge-function substrate decision in
`browser-node-proposal.md` §1.1/§2.3. Not a design claim — actual numbers from
running both candidate substrates in a real browser via the gm `browser` verb.

- **When:** 2026-07-31
- **Browser:** Chrome 150.0.0.0, macOS
- **Harness:** a sandboxed iframe (`sandbox="allow-scripts"`, **no**
  `allow-same-origin`, `srcdoc` guest, `postMessage` request/response protocol)
  vs `quickjs-emscripten@0.31.0`, both driven from one page served on loopback.
- **Raw data:** `browser-node-measurements.json` (committed alongside).

## Sandboxed iframe (the browser's own V8, opaque origin)

| Metric | Value |
| --- | --- |
| Frame boot (create → guest ready) | 122.9 ms |
| Function-invoke round-trip p50 | 0.2 ms |
| Function-invoke round-trip p95 | 6.8 ms |
| 5M-iteration compute loop | 56.2 ms |
| 100 KB payload round-trip | 1.9 ms |
| Isolation (witnessed) | `origin=null` (opaque); parent DOM, `localStorage`, `document.cookie` all `blocked:SecurityError` |

No in-place interrupt and no memory cap exist — the only hard kill is whole-frame
(or whole-Worker) teardown.

## QuickJS (quickjs-emscripten, interpreter in wasm)

| Metric | Value |
| --- | --- |
| One-time wasm init (per page) | 996.5 ms |
| Context boot (per function) | 1.8 ms |
| Eval latency p50 / p95 | 0.0 / 0.1 ms |
| 5M-iteration compute loop | 657.1 ms |
| Infinite loop contained (interrupt handler) | **yes**, in 1.2 ms |
| OOM contained (`setMemoryLimit(32 MB)`) | **yes** |

## Reading

QuickJS is the **only** substrate with witnessed hard containment (interrupt +
memory cap) — required for hostile multi-tenant functions — at ~11.7× the
interpreter cost (657 vs 56 ms on the compute loop). Per-instance boot is 68×
cheaper than an iframe (1.8 vs 122.9 ms) once the one-time wasm init is paid.

The sandboxed iframe gives full-speed V8, native web APIs at zero bundle cost,
and a browser-enforced origin boundary (proven above), but no in-place limits.

**Decision:** sandboxed-iframe V8 as the primary/fast lane for trusted &
first-party code; QuickJS-wasm nested inside the same frame as the metered lane
for untrusted tenant functions that need deterministic CPU/memory limits. See
`browser-node-proposal.md` §2.3.
