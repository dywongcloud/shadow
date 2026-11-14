/** The wire shape of GET /v1/admin/contract/explorer. Every verification field is a MEASUREMENT
 *  recomputed by replay of ONE node's RETAINED oplog window, never a stored "verified" flag.
 *  A server/client-differing value never seeds useState: this page passes the server view as props. */
export interface ExplorerEntry {
  seq: number;
  kind: string;
  contract: string;
  tenant: string;
  actor: string;
  target: string;
  payload: Record<string, string>;
  payload_omitted_keys: number;
  payload_truncated: boolean;
  declared_effect: "StateWrite" | "LogOnly" | "undeclared" | string;
  execution: {
    recorded_success: boolean;
    recorded_error: string | null;
    index_hash: string;
    log_head: string;
    ts_ms: number;
    recording: string;
  };
  entry_hash: string;
  prev_hash: string;
  recomputed_entry_hash: string;
  hash_ok: boolean;
  prev_link_ok: boolean;
  log_head_ok: boolean;
}

export interface ExplorerView {
  responding_node: string;
  scope: string;
  seq_note: string;
  canonical_note: string;
  public_key_hex: string;
  pin_required: boolean;
  contract: {
    id: string;
    version: number;
    declared_kinds: { kind: string; effect: string }[];
  };
  shard: {
    base: number;
    base_prev: string;
    next_seq: number;
    head: string;
    retained: number;
    compacted_prefix: boolean;
    full_history: boolean;
    compaction_note: string;
  };
  window: {
    returned: number;
    limit: number;
    from_seq: number | null;
    to_seq: number | null;
    has_more: boolean;
    next_before_seq: number | null;
  };
  verification: {
    kind: string;
    chain: {
      ok: boolean | null;
      entries_walked?: number;
      break?: { seq: number; expected: string; stored: string } | null;
      reason?: string;
    };
    index_replay: {
      available: boolean;
      ok?: boolean;
      breach?: string;
      recomputed_index_hash?: string;
      published_index_hash?: string;
      reason?: string;
    };
    entry_signature: { verified: boolean; reason: string };
    proves: string[];
    does_not_prove: string[];
    trust_limit: string;
  };
  entries: ExplorerEntry[];
  labels: Record<string, string>;
}

/** `fetchOpsResult` casts `data as T` without a runtime check. A 200 with a misshapen body must render
 *  ABSENT/error, never coerced. Validate the load-bearing shape once at the server boundary. */
export function isExplorerView(v: unknown): v is ExplorerView {
  if (!v || typeof v !== "object") return false;
  const d = v as Record<string, unknown>;
  return typeof d.responding_node === "string"
    && typeof d.scope === "string"
    && typeof d.seq_note === "string"
    && typeof d.canonical_note === "string"
    && typeof d.public_key_hex === "string"
    && typeof d.pin_required === "boolean"
    && isRecord(d.contract) && isRecord(d.shard) && isRecord(d.window)
    && isRecord(d.verification) && isRecord(d.labels)
    && Array.isArray(d.entries);
}

function isRecord(v: unknown): boolean {
  return !!v && typeof v === "object" && !Array.isArray(v);
}

export const EXPLORER_CONTRACTS = [
  "hive.managed-db.v1",
  "hive.billing-journal.v1",
  "hive.billing-meter.v1",
] as const;

/** A fixed allowlist: a caller-controlled string never reaches a URL by interpolation. */
export function explorerQuery(opts: {
  contract?: string | null;
  beforeSeq?: string | null;
  limit?: string | null;
}): string {
  const params = new URLSearchParams();
  const contract = EXPLORER_CONTRACTS.find((c) => c === opts.contract) ?? EXPLORER_CONTRACTS[1];
  params.set("contract", contract);
  const before = Number(opts.beforeSeq);
  if (Number.isSafeInteger(before) && before >= 0) params.set("before_seq", String(before));
  const limit = Number(opts.limit);
  if (Number.isSafeInteger(limit) && limit >= 1) params.set("limit", String(Math.min(limit, 100)));
  return params.toString();
}
