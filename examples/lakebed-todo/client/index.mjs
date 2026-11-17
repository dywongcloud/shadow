/**
 * Lakebed `lakebed/client`-shaped surface for the shadw-hosted Todo capsule.
 *
 * On Lakebed's hosted runtime the browser imports `createClient` from
 * `lakebed/client`; here the same call shape is implemented over the capsule's
 * own HTTP endpoints. The browser never holds a platform credential and never
 * names a storage URL: every read and write goes through the server-side
 * binding in `server/shadow.mjs`.
 */

export function createClient(base = "") {
  async function rpc(kind, name, args) {
    const response = await fetch(`${base}/api/${kind}/${name}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(args ?? {}),
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok || !payload.ok) {
      throw new Error(payload.message || payload.error || `HTTP ${response.status}`);
    }
    return payload.value;
  }

  return {
    /** Lakebed-shaped query: a callable that returns the measured rows. */
    useQuery(name, args) {
      return { name, args, run: () => rpc("query", name, args) };
    },
    /** Lakebed-shaped mutation: a callable that returns the written value. */
    useMutation(name) {
      return { name, run: (args) => rpc("mutation", name, args) };
    },
    /** Storage goes through the capsule, never directly to the platform. */
    storage: {
      async upload(bytes, options = {}) {
        const response = await fetch(`${base}/api/files`, { method: "POST", body: bytes });
        const payload = await response.json().catch(() => ({}));
        if (!response.ok) throw new Error(payload.message || `HTTP ${response.status}`);
        return { key: payload.key, size: payload.size, url: payload.url, ...options };
      },
      async get(key) {
        const response = await fetch(`${base}/api/files/${encodeURIComponent(key)}`);
        return response.ok ? response.text() : null;
      },
    },
  };
}

export function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

/** Mounts the demo onto the page. Kept dependency-free so no build step runs. */
export function mount(client, ownerId = "browser-demo") {
  const $ = (id) => document.getElementById(id);
  const errorEl = $("error");
  const report = (error) => { errorEl.textContent = error.message || String(error); errorEl.hidden = false; };

  const list = client.useQuery("list", { ownerId });
  const count = client.useQuery("count", { ownerId });
  const add = client.useMutation("add");
  const toggle = client.useMutation("toggle");
  const remove = client.useMutation("remove");

  async function refresh() {
    errorEl.hidden = true;
    try {
      const [todos, total] = await Promise.all([list.run(), count.run()]);
      $("count").textContent = `${total} item${total === 1 ? "" : "s"}`;
      $("todos").innerHTML = todos.length
        ? todos.map((t) => `<li class="${t.done ? "done" : ""}" data-id="${escapeHtml(t.id)}">
             <input type="checkbox" ${t.done ? "checked" : ""} aria-label="Toggle" />
             <span class="text">${escapeHtml(t.text)}</span>
             <button class="ghost remove" type="button">Delete</button></li>`).join("")
        : '<li class="meta">No todos yet.</li>';
    } catch (e) { report(e); }
  }

  $("add").addEventListener("submit", async (event) => {
    event.preventDefault();
    const text = $("text").value.trim();
    if (!text) return;
    try { await add.run({ text, ownerId }); $("text").value = ""; await refresh(); }
    catch (e) { report(e); }
  });

  $("todos").addEventListener("change", async (event) => {
    const li = event.target.closest("li[data-id]");
    if (!li || !event.target.matches('input[type="checkbox"]')) return;
    try { await toggle.run({ id: li.dataset.id }); await refresh(); }
    catch (e) { report(e); }
  });

  $("todos").addEventListener("click", async (event) => {
    const li = event.target.closest("li[data-id]");
    if (!li || !event.target.classList.contains("remove")) return;
    try { await remove.run({ id: li.dataset.id }); await refresh(); }
    catch (e) { report(e); }
  });

  $("refresh").addEventListener("click", refresh);

  $("upload").addEventListener("submit", async (event) => {
    event.preventDefault();
    try {
      const object = await client.storage.upload($("payload").value);
      const back = await client.storage.get(object.key);
      $("file").textContent = `Round trip: ${object.size} bytes written, ${new Blob([back ?? ""]).size} bytes read back.`;
    } catch (e) { report(e); }
  });

  return refresh();
}
