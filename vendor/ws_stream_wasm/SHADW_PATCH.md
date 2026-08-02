# ws_stream_wasm 0.7.5 local patch

Source: <https://crates.io/crates/ws_stream_wasm/0.7.5> (Unlicense).

The upstream connection path waits for WebSocket `Open` before selecting
`binaryType = "arraybuffer"` and installing its `onmessage` receiver. Firefox
can deliver an iroh relay's first binary frame in that interval, either as the
default `Blob` or before any receiver exists. The relay handshake then waits
forever for a frame that was discarded.

This copy selects the binary type immediately after socket construction and
transfers callback ownership to `WsStream` before awaiting `Open`. Remove the
patch when an upstream release contains both ordering fixes.
