// A @workflow/world implementation that proxies every read (and the four run operations) to
// hive's /v1/workflows/* admin API, loaded by the compiled @workflow/web server bundle via
// WORKFLOW_TARGET_WORLD; per-request team + bearer ride an AsyncLocalStorage (see ALS_KEY).

import { AsyncLocalStorage } from "node:async_hooks";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const DEFAULT_PROJECT = process.env.HIVE_WF_PROJECT || "";
const DEFAULT_TEAM = process.env.HIVE_WF_TEAM || "personal";
const TIMEOUT_MS = Number(process.env.HIVE_WF_TIMEOUT_MS || 25_000);

const ALS_KEY = Symbol.for("hive.wfConsole.requestContext");
if (!globalThis[ALS_KEY]) globalThis[ALS_KEY] = new AsyncLocalStorage();
/** @type {AsyncLocalStorage<{team?: string, authToken?: string, project?: string}>} */
const als = globalThis[ALS_KEY];

function ctx() {
  return als.getStore() || {};
}

/** The active project scope, or `undefined` when unscoped (global view). */
function currentProject() {
  return ctx().project || DEFAULT_PROJECT || undefined;
}

/** `&project=<p>` (or "") for endpoints that fan out tenant-wide without it. */
function projectParam() {
  const p = currentProject();
  return p ? `&project=${encodeURIComponent(p)}` : "";
}

/** `{ project }` spread for run-op bodies — omitted = backend auto-scan. */
function projectBody() {
  const p = currentProject();
  return p ? { project: p } : {};
}

async function hive(path, init) {
  const method = (init && init.method) || "GET";
  try {
    return await hiveOnce(path, init);
  } catch (e) {
    const aborted = e && (e.name === "AbortError" || /aborted/i.test(String(e.message || "")));
    if (method === "GET" && aborted) {
      return await hiveOnce(path, init);
    }
    throw e;
  }
}

async function hiveOnce(path, init) {
  const c = ctx();
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(), TIMEOUT_MS);
  try {
    const res = await fetch(`${ADMIN}${path}`, {
      ...init,
      headers: {
        "content-type": "application/json",
        "x-hive-team": c.team || DEFAULT_TEAM,
        ...(c.authToken ? { authorization: `Bearer ${c.authToken}` } : {}),
        ...(init && init.headers ? init.headers : {}),
      },
      cache: "no-store",
      signal: ctrl.signal,
    });
    if (!res.ok) {
      const text = await res.text().catch(() => "");
      const err = new Error(
        `hive ${init && init.method ? init.method : "GET"} ${path} -> ${res.status} ${text.slice(0, 300)}`
      );
      err.status = res.status;
      throw err;
    }
    return await res.json();
  } finally {
    clearTimeout(t);
  }
}

const RUN_STATUSES = ["pending", "running", "completed", "failed", "cancelled"];
const STATUS_ALIASES = {
  succeeded: "completed",
  success: "completed",
  errored: "failed",
  error: "failed",
  canceled: "cancelled",
};

function mapStatus(s) {
  const v = String(s || "pending").toLowerCase();
  if (RUN_STATUSES.includes(v)) return v;
  return STATUS_ALIASES[v] || "pending";
}

const TERMINAL = new Set(["completed", "failed", "cancelled"]);

/** hive timestamps are epoch-seconds floats OR epoch-millis ints — normalize. */
function toDate(v) {
  if (v === null || v === undefined) return undefined;
  if (v instanceof Date) return v;
  const n = Number(v);
  if (Number.isFinite(n)) {
    if (n <= 0) return undefined;
    return new Date(n < 1e12 ? Math.round(n * 1000) : Math.round(n));
  }
  const d = new Date(v);
  return Number.isNaN(d.getTime()) ? undefined : d;
}

const DEVALUE_MAGIC = Buffer.from("devl");

/** Decode hive's base64 devalue payloads (`DEVALUE_MAGIC` prefix) to the Uint8Array the console
 *  expects; legacy values pass through untouched. */
function decodeSerialized(v) {
  if (v === null || v === undefined) return undefined;
  if (typeof v !== "string") return v;
  try {
    const buf = Buffer.from(v, "base64");
    if (
      buf.length >= DEVALUE_MAGIC.length &&
      buf.subarray(0, DEVALUE_MAGIC.length).equals(DEVALUE_MAGIC)
    ) {
      return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
    }
  } catch {}
  return v;
}

function reshapeRun(r, resolveData = "all") {
  if (!r || typeof r !== "object") return r;
  const status = mapStatus(r.status);
  const createdAt = toDate(r.createdAt ?? r.started_ms) || new Date(0);
  const out = {
    runId: r.runId || r.id,
    status,
    deploymentId: r.deploymentId || "unknown",
    workflowName: r.workflowName || r.name || "workflow//unknown//unknown",
    specVersion: typeof r.specVersion === "number" ? r.specVersion : undefined,
    executionContext:
      r.executionContext && typeof r.executionContext === "object"
        ? r.executionContext
        : undefined,
    attributes:
      r.attributes && typeof r.attributes === "object" ? r.attributes : {},
    createdAt,
    updatedAt: toDate(r.updatedAt) || toDate(r.completedAt) || createdAt,
    startedAt: toDate(r.startedAt ?? r.started_ms),
  };
  if (TERMINAL.has(status)) {
    out.completedAt =
      toDate(r.completedAt ?? r.finished_ms) || out.updatedAt || new Date();
  }
  if (r.errorCode) out.errorCode = r.errorCode;
  if (r.replayedFromRunId) out.replayedFromRunId = r.replayedFromRunId;
  if (resolveData === "all") {
    const input = decodeSerialized(r.input);
    if (input !== undefined) out.input = input;
    if (status === "completed") {
      const output = decodeSerialized(r.output);
      if (output !== undefined) out.output = output;
    }
    if (status === "failed") {
      const error = decodeSerialized(r.error);
      if (error !== undefined) out.error = error;
    }
  } else {
    out.input = undefined;
    out.output = undefined;
  }
  return out;
}

function reshapeStep(s, resolveData = "all") {
  if (!s || typeof s !== "object") return s;
  const createdAt = toDate(s.createdAt) || new Date(0);
  const out = {
    runId: s.runId,
    stepId: s.stepId,
    stepName: s.stepName || "step//unknown//unknown",
    status: mapStatus(s.status),
    attempt: typeof s.attempt === "number" ? s.attempt : 1,
    createdAt,
    updatedAt: toDate(s.updatedAt) || createdAt,
    startedAt: toDate(s.startedAt),
    completedAt: toDate(s.completedAt),
    retryAfter: toDate(s.retryAfter),
    specVersion: typeof s.specVersion === "number" ? s.specVersion : undefined,
  };
  if (resolveData === "all") {
    const input = decodeSerialized(s.input);
    if (input !== undefined) out.input = input;
    const output = decodeSerialized(s.output);
    if (output !== undefined) out.output = output;
    const error = decodeSerialized(s.error);
    if (error !== undefined) out.error = error;
  } else {
    out.input = undefined;
    out.output = undefined;
  }
  return out;
}

const EVENT_PAYLOAD_FIELD = {
  run_created: "input",
  run_started: "input",
  run_completed: "output",
  run_failed: "error",
  step_created: "input",
  step_started: "input",
  step_completed: "result",
  step_failed: "error",
  step_retrying: "error",
  hook_created: "metadata",
  hook_received: "payload",
};

function reshapeEvent(e, withData) {
  if (!e || typeof e !== "object") return e;
  const out = {
    eventId: e.eventId,
    runId: e.runId,
    eventType: e.eventType,
    createdAt: toDate(e.createdAt) || new Date(0),
  };
  if (typeof e.specVersion === "number") out.specVersion = e.specVersion;
  if (e.correlationId) out.correlationId = e.correlationId;
  const ed = e.eventData;
  if (ed && typeof ed === "object") {
    const copy = { ...ed };
    const payloadField = EVENT_PAYLOAD_FIELD[e.eventType];
    if (payloadField && payloadField in copy) {
      if (withData) copy[payloadField] = decodeSerialized(copy[payloadField]);
      else delete copy[payloadField];
    }
    if ("resumeAt" in copy) copy.resumeAt = toDate(copy.resumeAt);
    if (Object.keys(copy).length > 0) out.eventData = copy;
  } else if (ed !== undefined && ed !== null) {
    out.eventData = ed;
  }
  return out;
}

function reshapeHook(h) {
  if (!h || typeof h !== "object") return h;
  return {
    runId: h.runId || "",
    hookId: h.hookId || h.id || "",
    token: h.token || "",
    ownerId: h.ownerId || "",
    projectId: h.projectId || currentProject() || "",
    environment: h.environment || "production",
    metadata: decodeSerialized(h.metadata),
    createdAt: toDate(h.createdAt) || new Date(0),
    specVersion: typeof h.specVersion === "number" ? h.specVersion : undefined,
    isWebhook: h.isWebhook === true ? true : undefined,
    isSystem: h.isSystem === true ? true : undefined,
  };
}

function parseWfName(full) {
  const parts = String(full || "").split("//");
  if (parts.length >= 3) {
    return { filePath: parts[1], name: parts[parts.length - 1] };
  }
  return { filePath: "./workflows", name: String(full || "workflow") };
}

function paginate(rows, pagination = {}, defaultSort = "desc", sortKey = "createdAt") {
  const sortOrder = pagination.sortOrder || defaultSort;
  const limit = Math.max(1, Math.min(1000, Number(pagination.limit) || 100));
  const offset = Math.max(0, Number.parseInt(pagination.cursor || "0", 10) || 0);
  const sorted = [...rows].sort((a, b) => {
    const ta = a && a[sortKey] instanceof Date ? a[sortKey].getTime() : 0;
    const tb = b && b[sortKey] instanceof Date ? b[sortKey].getTime() : 0;
    return sortOrder === "asc" ? ta - tb : tb - ta;
  });
  const page = sorted.slice(offset, offset + limit);
  const hasMore = offset + limit < sorted.length;
  return {
    data: page,
    cursor: hasMore ? String(offset + limit) : null,
    hasMore,
  };
}

const DETAIL_TTL_MS = 2_000;
const detailCache = new Map();
const correlationToRun = new Map();
const CORRELATION_CAP = 20_000;

function rememberCorrelations(runId, events) {
  for (const e of events || []) {
    if (e && e.correlationId) {
      if (correlationToRun.size > CORRELATION_CAP) correlationToRun.clear();
      correlationToRun.set(e.correlationId, runId);
    }
  }
}

async function fetchRunDetail(runId) {
  const hit = detailCache.get(runId);
  if (hit && Date.now() - hit.at < DETAIL_TTL_MS) return hit.promise;
  const promise = (async () => {
    const id = encodeURIComponent(runId);
    const project = currentProject();
    let detail;
    if (project) {
      try {
        detail = await hive(`/v1/workflows/runs/${id}?project=${encodeURIComponent(project)}`);
      } catch (e) {
        detail = await hive(`/v1/workflows/runs/${id}`);
      }
    } else {
      detail = await hive(`/v1/workflows/runs/${id}`);
    }
    const run = detail && detail.run;
    if (!run || run === null) {
      const err = new Error(`Workflow run ${runId} not found`);
      err.status = 404;
      throw err;
    }
    rememberCorrelations(runId, detail.events);
    return detail;
  })();
  detailCache.set(runId, { at: Date.now(), promise });
  promise.catch(() => detailCache.delete(runId));
  return promise;
}

function buildWorld() {
  const world = {
    specVersion: 3,

    runs: {
      async get(runId, params) {
        const resolveData = (params && params.resolveData) || "all";
        const detail = await fetchRunDetail(runId);
        return reshapeRun(detail.run, resolveData);
      },
      async list(params = {}) {
        const rows = await hive(`/v1/workflows/runs?summary=1${projectParam()}`);
        let runs = (Array.isArray(rows) ? rows : []).map((r) =>
          reshapeRun(r, "none")
        );
        if (params.workflowName) {
          runs = runs.filter((r) => r.workflowName === params.workflowName);
        }
        if (params.status) {
          const want = mapStatus(params.status);
          runs = runs.filter((r) => r.status === want);
        }
        return paginate(runs, params.pagination, "desc");
      },
    },

    steps: {
      async get(runId, stepId, params) {
        const resolveData = (params && params.resolveData) || "all";
        const detail = await fetchRunDetail(runId);
        const row = (detail.steps || []).find((s) => s && s.stepId === stepId);
        if (!row) {
          const err = new Error(`Step ${stepId} not found on run ${runId}`);
          err.status = 404;
          throw err;
        }
        return reshapeStep(row, resolveData);
      },
      async list(params = {}) {
        const resolveData = params.resolveData || "none";
        const detail = await fetchRunDetail(params.runId);
        const steps = (detail.steps || []).map((s) => reshapeStep(s, resolveData));
        return paginate(steps, params.pagination, "asc");
      },
    },

    events: {
      async get(runId, eventId) {
        const detail = await fetchRunDetail(runId);
        const row = (detail.events || []).find((e) => e && e.eventId === eventId);
        if (!row) {
          const err = new Error(`Event ${eventId} not found on run ${runId}`);
          err.status = 404;
          throw err;
        }
        return reshapeEvent(row, true);
      },
      async list(params = {}) {
        const withData = params.resolveData === "all";
        const detail = await fetchRunDetail(params.runId);
        const events = (detail.events || []).map((e) => reshapeEvent(e, withData));
        return paginate(events, params.pagination, "asc");
      },
      async listByCorrelationId(params = {}) {
        const withData = params.resolveData === "all";
        const runId = correlationToRun.get(params.correlationId);
        if (!runId) return { data: [], cursor: null, hasMore: false };
        const detail = await fetchRunDetail(runId);
        const events = (detail.events || [])
          .filter((e) => e && e.correlationId === params.correlationId)
          .map((e) => reshapeEvent(e, withData));
        return paginate(events, params.pagination, "asc");
      },
      async create(runId, data) {
        const eventType = data && data.eventType;
        if (eventType === "run_cancelled") {
          await world.hiveOps.cancelRun(runId);
          return {
            event: {
              eventId: `evnt_local_${Date.now()}`,
              runId,
              eventType,
              createdAt: new Date(),
            },
            entity: undefined,
          };
        }
        if (eventType === "wait_completed") {
          return {
            event: {
              eventId: `evnt_local_${Date.now()}`,
              runId,
              eventType,
              correlationId: data.correlationId,
              createdAt: new Date(),
            },
            entity: undefined,
          };
        }
        throw new Error(
          `hive world is read-only (attempted events.create ${String(eventType)})`
        );
      },
    },

    hooks: {
      async get(hookId) {
        const hooks = await listAllHooks();
        const hook = hooks.find((h) => h.hookId === hookId);
        if (!hook) {
          const err = new Error(`Hook ${hookId} not found`);
          err.status = 404;
          throw err;
        }
        return hook;
      },
      async getByToken(token) {
        const hooks = await listAllHooks();
        const hook = hooks.find((h) => h.token === token);
        if (!hook) {
          const err = new Error(`Hook with token not found`);
          err.status = 404;
          throw err;
        }
        return hook;
      },
      async list(params = {}) {
        const hooks = await listAllHooks(params.runId);
        return paginate(hooks, params.pagination, "desc");
      },
    },

    async listStreamsByRunId() {
      return [];
    },
    streamFlushIntervalMs: 0,
    streams: {
      async write() {
        throw new Error("hive world is read-only (streams.write)");
      },
      async close() {
        throw new Error("hive world is read-only (streams.close)");
      },
      async get() {
        return new ReadableStream({
          start(controller) {
            controller.close();
          },
        });
      },
      async list() {
        return [];
      },
      async getChunks() {
        return { data: [], cursor: null, hasMore: false, done: true };
      },
      async getInfo() {
        return { tailIndex: -1, done: true };
      },
    },

    async queue(_queueName, payload) {
      const runId = payload && payload.runId;
      if (!runId) throw new Error("hive world queue(): missing runId");
      await world.hiveOps.reenqueueRun(runId);
      return { messageId: `msg_local_${Date.now()}` };
    },

    createQueueHandler(_queueNamePrefix, _handler) {
      return async () =>
        new Response(
          JSON.stringify({ error: "hive wf-console does not process queue messages" }),
          { status: 501, headers: { "content-type": "application/json" } }
        );
    },

    async getDeploymentId() {
      return "dpl_hive_console";
    },

    hiveOps: {
      async cancelRun(runId, cancelReason) {
        return hive(`/v1/workflows/runs/${encodeURIComponent(runId)}/cancel`, {
          method: "POST",
          body: JSON.stringify({
            ...projectBody(),
            ...(cancelReason ? { cancelReason } : {}),
          }),
        });
      },
      async recreateRun(runId) {
        const v = await hive(
          `/v1/workflows/runs/${encodeURIComponent(runId)}/replay`,
          {
            method: "POST",
            body: JSON.stringify(projectBody()),
          }
        );
        return (v && (v.runId || v.run_id || v.newRunId)) || String(v ?? "");
      },
      async reenqueueRun(runId) {
        return hive(
          `/v1/workflows/runs/${encodeURIComponent(runId)}/reenqueue`,
          {
            method: "POST",
            body: JSON.stringify(projectBody()),
          }
        );
      },
      async wakeUpRun(runId) {
        const v = await hive(
          `/v1/workflows/runs/${encodeURIComponent(runId)}/wakeup`,
          {
            method: "POST",
            body: JSON.stringify(projectBody()),
          }
        );
        return {
          stoppedCount:
            (v && (v.stoppedCount ?? v.stopped_count ?? v.stopped)) || 0,
        };
      },
    },

    async hiveFetchWorkflowsManifest() {
      const workflows = {};
      const addEntry = (fullName, graph) => {
        const { filePath, name } = parseWfName(fullName);
        if (!workflows[filePath]) workflows[filePath] = {};
        if (!workflows[filePath][name]) {
          workflows[filePath][name] = {
            workflowId: String(fullName),
            graph:
              graph && Array.isArray(graph.nodes) && Array.isArray(graph.edges)
                ? graph
                : { nodes: [], edges: [] },
          };
        }
      };
      try {
        const p = currentProject();
        const defs = await hive(p ? `/v1/workflows?project=${encodeURIComponent(p)}` : `/v1/workflows`);
        for (const d of Array.isArray(defs) ? defs : []) {
          const full = d.id && String(d.id).includes("//") ? d.id : d.name;
          addEntry(full || d.id || d.name, d.graph);
        }
      } catch {}
      if (Object.keys(workflows).length === 0) {
        try {
          const rows = await hive(
            `/v1/workflows/runs?summary=1${projectParam()}`
          );
          for (const r of Array.isArray(rows) ? rows : []) {
            if (r && (r.workflowName || r.name)) {
              addEntry(r.workflowName || r.name, null);
            }
          }
        } catch {}
      }
      return { version: "1.0.0", workflows, steps: {} };
    },
  };
  return world;
}

let cached = null;

export function createWorld() {
  if (!cached) cached = buildWorld();
  return cached;
}

async function listAllHooks(runId) {
  const project = encodeURIComponent(currentProject());
  const rid = runId ? `&runId=${encodeURIComponent(runId)}` : "";
  const rows = await hive(`/v1/workflows/hooks?project=${project}${rid}`);
  return (Array.isArray(rows) ? rows : []).map(reshapeHook);
}

export default createWorld;
