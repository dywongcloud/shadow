// Express, run under Bun via a `"start": "node server.js"` package.json
// script — the single most common real-world shape (it's what `npm init`
// emits). This exact fixture caught a real bug: the platform's
// `detect_start_cmd` used to emit `["bun","run","start"]` for this shape,
// which runs the script's OWN TEXT ("node server.js") as a shell command —
// Bun's script-runner spawned REAL Node, silently defeating the Bun runtime
// choice (process.versions.bun was null). Fixed by adding `--bun`
// (`["bun","run","--bun","start"]`), which forces Bun to substitute itself for
// any node-shebang child process the script invokes. Manually verified live
// both ways: without `--bun`, `bun: null`; with it, the real Bun version.
const express = require("express");
const app = express();
app.use(express.json());

app.get("/", (req, res) => res.json({ msg: "hello from an Express Fluid function", bun: process.versions.bun ?? null }));

app.get("/api/boom", () => {
  throw new Error("intentional boom"); // must not take down other requests
});

const port = process.env.PORT || 8000;
app.listen(port, "127.0.0.1", () => console.log(`listening on ${port}`));
