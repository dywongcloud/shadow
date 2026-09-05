//! Tamper-**evidence** for a deployment's build/publish/execution history —
//! not tamper-prevention, and not hardware-rooted confidential computing
//! (except the narrow Mac-node Secure Enclave signing step this vocabulary
//! also describes, see `hive-backend::sep_signer`).
//!
//! **What this proves:** that a deployment's recorded history — which build
//! was accepted, which artifact was published, which node ran it and when —
//! was not silently altered after each entry was written. A verifier
//! recomputes [`fold_integrity_chain`] over the entries a node returns and
//! checks it matches the node's claimed chain head, then checks the node's
//! signature over that head against its published public key.
//!
//! **What this does NOT prove:** that the recorded execution was itself
//! uncompromised in real time (a root user can still tamper with live memory
//! while a workload runs), or that the isolation backend that ran it was
//! strong (a `Mock`-backend entry is exactly as tamper-evident, and exactly
//! as silent about in-process tampering, as a `Firecracker` one — the chain
//! records WHAT ran, never HOW STRONGLY it was isolated while running). On
//! Linux nodes the signing key is software, node-local, and — like any
//! software key a root user controls — extractable; a compromised Linux node
//! could in principle both alter history AND forge a self-consistent
//! signature over the altered version. On the Mac node, entries signed with
//! a Secure-Enclave-resident per-deployment key carry a categorically
//! stronger guarantee for exactly that one failure mode: the private key
//! cannot be extracted even by a root user on that box, so a compromised Mac
//! node can tamper with a *running* workload but cannot retroactively forge
//! a legitimate-looking signature over an altered history for entries it
//! already signed. That asymmetry is real and must never be described as
//! uniform "verifiable compute" across the fleet.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// ASCII unit separator — delimits chain-hash fields without needing a
/// canonicalized (JSON/CBOR) encoding, same choice as
/// `hive-cloud::billing::compute_ledger_checkpoint`.
const SEP: u8 = 0x1F;

/// One append-only entry in a single deployment's tamper-evidence chain.
/// `kind` domain-separates build-provenance, publish, execution, and
/// Secure-Enclave-key-provisioning facts inside one shared chain type.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrityEntry {
    pub deployment_id: String,
    /// Strictly increasing per deployment, gap-free — the chain's ordering
    /// key (unlike the billing ledger, this chain is single-deployment and
    /// single-writer-per-node, so `seq` — not `(ts_ms, id)` — is the natural
    /// strict order).
    pub seq: u64,
    pub ts_ms: u64,
    pub node: String,
    pub kind: IntegrityEntryKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum IntegrityEntryKind {
    BuildAccepted {
        source_revision_sha256: String,
        runtime_artifact_content_sha256: String,
        repository_build_sha256: Option<String>,
    },
    Published {
        alias: String,
    },
    ExecutionStarted {
        cell_id: String,
        /// "firecracker" | "litebox" | "mock" — verbatim from `NodeInfo`'s
        /// existing `backend_name`, never re-derived.
        backend: String,
    },
    ExecutionEnded {
        cell_id: String,
        requests_served: u64,
        /// "drained" | "crashed" | "killed"
        outcome: String,
    },
    /// Mac-node only: the public half of a Secure-Enclave-resident,
    /// non-exportable per-deployment signing key was provisioned. Published
    /// here (self-describing, content-addressed by the chain itself) rather
    /// than in a separate registry — a verifier checking a Mac-authored
    /// entry's signature finds the matching public key in this same chain.
    SepKeyProvisioned {
        public_key_der_hex: String,
        key_tag: String,
    },
}

impl IntegrityEntryKind {
    fn tag(&self) -> &'static str {
        match self {
            IntegrityEntryKind::BuildAccepted { .. } => "build_accepted",
            IntegrityEntryKind::Published { .. } => "published",
            IntegrityEntryKind::ExecutionStarted { .. } => "execution_started",
            IntegrityEntryKind::ExecutionEnded { .. } => "execution_ended",
            IntegrityEntryKind::SepKeyProvisioned { .. } => "sep_key_provisioned",
        }
    }

    /// Canonical byte payload folded into this entry's leaf hash — every
    /// field of the variant, `0x1F`-delimited, in declaration order. Kept
    /// separate from `tag()` so two different kinds never collide even if
    /// their field values happen to match byte-for-byte.
    fn payload_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        macro_rules! push {
            ($s:expr) => {
                if !out.is_empty() {
                    out.push(SEP);
                }
                out.extend_from_slice($s.as_bytes());
            };
        }
        match self {
            IntegrityEntryKind::BuildAccepted {
                source_revision_sha256,
                runtime_artifact_content_sha256,
                repository_build_sha256,
            } => {
                push!(source_revision_sha256);
                push!(runtime_artifact_content_sha256);
                push!(repository_build_sha256.as_deref().unwrap_or(""));
            }
            IntegrityEntryKind::Published { alias } => {
                push!(alias);
            }
            IntegrityEntryKind::ExecutionStarted { cell_id, backend } => {
                push!(cell_id);
                push!(backend);
            }
            IntegrityEntryKind::ExecutionEnded {
                cell_id,
                requests_served,
                outcome,
            } => {
                push!(cell_id);
                push!(requests_served.to_string());
                push!(outcome);
            }
            IntegrityEntryKind::SepKeyProvisioned {
                public_key_der_hex,
                key_tag,
            } => {
                push!(public_key_der_hex);
                push!(key_tag);
            }
        }
        out
    }
}

/// hex(SHA-256) chain root over `entries`, exact-shape analog of
/// `hive-cloud::billing::compute_ledger_checkpoint`: a versioned,
/// deployment-scoped genesis hash, then one SHA-256 fold per entry in `seq`
/// ASC order. Callers MUST pass entries already sorted by `seq` — this
/// function does not sort, so it can be used identically whether the caller
/// holds the full history or a bounded window (verification needs a caller
/// that recomputes from a known starting point, e.g. a previous known-good
/// `chain_head`).
pub fn fold_integrity_chain(deployment_id: &str, entries: &[IntegrityEntry]) -> String {
    let mut chain = Sha256::new();
    chain.update(b"hive-integrity-chain-v1");
    chain.update([SEP]);
    chain.update(deployment_id.as_bytes());
    let mut chain = chain.finalize();

    for e in entries {
        let mut leaf = Sha256::new();
        leaf.update(e.seq.to_string().as_bytes());
        leaf.update([SEP]);
        leaf.update(e.ts_ms.to_string().as_bytes());
        leaf.update([SEP]);
        leaf.update(e.kind.tag().as_bytes());
        leaf.update([SEP]);
        leaf.update(e.node.as_bytes());
        leaf.update([SEP]);
        leaf.update(e.kind.payload_bytes());
        let leaf = leaf.finalize();

        let mut next = Sha256::new();
        next.update(chain);
        next.update([SEP]);
        next.update(leaf);
        chain = next.finalize();
    }

    hex::encode(chain)
}

/// Injected by `hive-cloud` into `fluid_compute::Pool` at construction, so
/// the execution hot path can record `ExecutionStarted`/`ExecutionEnded`
/// facts into the integrity chain without `fluid-compute` depending on
/// `hive-cloud` (the crate DAG is one-directional: `hive-cloud` depends on
/// `fluid-compute`, never the reverse — the same "coordinator injects,
/// backend implements" shape `Arc<dyn CellBackend>` already uses).
pub trait ExecutionObserver: Send + Sync {
    fn on_started(&self, deployment_id: &str, cell_id: &str, backend: &str);
    fn on_ended(&self, deployment_id: &str, cell_id: &str, requests_served: u64, outcome: &str);
}
