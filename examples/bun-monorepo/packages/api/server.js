const { greeting } = require("@acme/shared");
Bun.serve({
  port: Number(process.env.PORT || 8000),
  fetch() {
    return new Response(JSON.stringify({ msg: greeting(), bun: process.versions.bun ?? null }), {
      headers: { "content-type": "application/json" },
    });
  },
});
console.log("listening");
