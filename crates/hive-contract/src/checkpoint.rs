//! Signed checkpoints: the compact artefact that bounds the log and makes it auditable.
//!
//! The oplog is append-only and unbounded, so on its own it grows forever and every audit costs a
//! replay from genesis. A checkpoint is the answer to both: it is an attested statement, signed by
//! the node's existing Ed25519 identity, that says
//!
//! > for contract C, after applying entries `[window_base, window_end)`, the index had hash
//! > `index_hash` and the log's chain head was `log_head`; the window's entries commit to
//! > `merkle_root`; and the head BEFORE the window was `prev_head`.
//!
//! That one statement replaces two things a verifier would otherwise need the whole log for:
//!   * **State.** Replaying from a checkpoint starts from `index` and applies only the entries after
//!     `window_end`, so audit cost stops being O(history).
//!   * **History it cannot see.** `prev_head` and `log_head` bind the window into the chain, so a
//!     verifier that never held the dropped entries can still tell whether the signed window is the
//!     one that actually preceded the entries it does hold.
//!
//! Everything the signature covers is recomputable by a third party. Nothing about it requires
//! trusting the signer beyond "this public key is the node's": the signature is checked,
//! `index_hash` is re-derived from the stored `index`, and `merkle_root` is what an
//! [`crate::merkle::MerklePath`] is verified against.
//!
//! `prev_head` is the field that makes truncation safe. Dropping the entries before a window loses
//! the ability to recompute them -- but the checkpoint carries the head they ended at, and the first
//! retained entry links to exactly that head, so the chain is continuous across the cut.

use crate::exec::{index_hash, Index};
use crate::merkle::{self, MerklePath, VerifyError as MerkleVerifyError};
use crate::oplog::{canonical_fields, LogEntry};
use crate::signing::{verify_ed25519, CheckpointSigner};

/// Domain separator for the bytes a checkpoint signature covers.
pub const CHECKPOINT_DOMAIN: &[u8] = b"hive-contract/checkpoint/v1";

/// The attested statement, unsigned. Serialized whole, because a checkpoint is a document a third
/// party receives, not an in-memory convenience.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    pub contract_id: String,
    pub contract_version: u32,
    /// First seq of the window: the entries the checkpoint's `index` already reflects.
    pub window_base: u64,
    /// One past the last seq of the window. Replay from this checkpoint resumes HERE.
    pub window_end: u64,
    /// Log head before the window: `prev_hash` of the entry at `window_base`. Carried so a truncated
    /// log can still be chained.
    pub prev_head: String,
    /// Log head after the window: the head the entry at `window_end` links to.
    pub log_head: String,
    /// [`crate::merkle::merkle_root`] over the window's entry hashes: what an inclusion path is
    /// verified against.
    pub merkle_root: String,
    /// [`crate::exec::index_hash`] of `index`. The signature covers this, and `index` is checked
    /// against it on every verify, so the two can never disagree.
    pub index_hash: String,
    /// The full index at `window_end`. It is what makes replay from a checkpoint O(tail) instead of
    /// O(history), and it is the only part of a checkpoint that grows with the number of databases.
    pub index: Index,
    pub ts_ms: u64,
}

impl Checkpoint {
    /// The bytes the signature covers. Every field that decides what the statement MEANS is in here;
    /// `index` is not, because `index_hash` already commits to it byte for byte.
    ///
    /// Built from [`canonical_fields`] with an explicit separator rather than from
    /// `serde_json`, whose map ordering depends on a cargo feature another workspace member could
    /// flip -- the same reason the oplog's hashes are hand-rolled.
    pub fn canonical(&self) -> Vec<u8> {
        canonical_fields(&[
            &self.contract_id,
            &self.contract_version.to_string(),
            &self.window_base.to_string(),
            &self.window_end.to_string(),
            &self.prev_head,
            &self.log_head,
            &self.merkle_root,
            &self.index_hash,
            &self.ts_ms.to_string(),
        ])
    }

    /// Whether `seq` is one of the entries this checkpoint's window covers.
    pub fn covers(&self, seq: u64) -> bool {
        seq >= self.window_base && seq < self.window_end
    }
}

/// Why a checkpoint could not be signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignError {
    /// The signer refused or failed. Never turned into an unsigned artefact: an unsigned checkpoint
    /// asserts nothing, and an artefact that looks like a checkpoint but carries no signature is how
    /// a forged history would be smuggled in.
    SignerFailed(String),
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignError::SignerFailed(d) => write!(f, "checkpoint signing failed: {d}"),
        }
    }
}

/// A checkpoint plus the signature that binds it. This is the artefact that leaves the node.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SignedCheckpoint {
    pub checkpoint: Checkpoint,
    pub signer_id: String,
    /// Hex Ed25519 public key: a verifier needs the key the signature was made with, and carrying it
    /// means a rotation is visible rather than silent.
    pub public_key_hex: String,
    pub signature_hex: String,
    pub signed_ms: u64,
}

impl SignedCheckpoint {
    /// Sign a checkpoint. The signature is over [`Checkpoint::canonical`], not over the serialized
    /// document, so a re-serialization (pretty-printed, reordered, written by another version) does
    /// not invalidate it.
    pub fn sign(
        checkpoint: Checkpoint,
        signer: &dyn CheckpointSigner,
        signed_ms: u64,
    ) -> Result<SignedCheckpoint, SignError> {
        let sig = signer
            .sign(&checkpoint.canonical())
            .map_err(SignError::SignerFailed)?;
        Ok(SignedCheckpoint {
            checkpoint,
            signer_id: signer.signer_id().to_string(),
            public_key_hex: hex::encode(signer.public_key()),
            signature_hex: hex::encode(sig),
            signed_ms,
        })
    }

    /// Verify the artefact end to end, with no trust in the producer beyond its public key.
    ///
    /// Three independent checks, all of which a third party can run on a checkpoint it was merely
    /// handed:
    ///   1. the stored `index` really hashes to `index_hash` (the index is not a lie about itself);
    ///   2. the signature over [`Checkpoint::canonical`] verifies under the carried public key;
    ///   3. an optional inclusion path really lands on `merkle_root`.
    pub fn verify(&self) -> Result<(), CheckpointVerifyError> {
        self.verify_index()?;
        self.verify_signature()
    }

    /// Check 1: the index the artefact carries is the index it names.
    pub fn verify_index(&self) -> Result<(), CheckpointVerifyError> {
        let computed = index_hash(&self.checkpoint.index);
        if computed != self.checkpoint.index_hash {
            return Err(CheckpointVerifyError::IndexHashMismatch {
                window_end: self.checkpoint.window_end,
                published: self.checkpoint.index_hash.clone(),
                computed,
            });
        }
        Ok(())
    }

    /// Check 2: the signature verifies under the key the artefact carries.
    pub fn verify_signature(&self) -> Result<(), CheckpointVerifyError> {
        let pk =
            hex::decode(&self.public_key_hex).map_err(|_| CheckpointVerifyError::BadPublicKey {
                window_end: self.checkpoint.window_end,
            })?;
        let sig =
            hex::decode(&self.signature_hex).map_err(|_| CheckpointVerifyError::BadSignature {
                window_end: self.checkpoint.window_end,
            })?;
        if !verify_ed25519(&pk, &self.checkpoint.canonical(), &sig) {
            return Err(CheckpointVerifyError::BadSignature {
                window_end: self.checkpoint.window_end,
            });
        }
        Ok(())
    }

    /// Check 3: `entry` is inside the window this checkpoint commits to.
    ///
    /// `path` must have been cut against this window's entries and `entry` must be the entry at
    /// `path.index`; the proof binds the hash, and the caller binds the hash to the entry by
    /// recomputing `entry_hash` from `entry.op`.
    pub fn verify_inclusion(
        &self,
        entry: &LogEntry,
        path: &MerklePath,
    ) -> Result<(), CheckpointVerifyError> {
        let seq = path.index.checked_add(self.checkpoint.window_base).ok_or(
            CheckpointVerifyError::OutsideWindow {
                seq: u64::MAX,
                base: self.checkpoint.window_base,
                end: self.checkpoint.window_end,
            },
        )?;
        if !self.checkpoint.covers(seq) {
            return Err(CheckpointVerifyError::OutsideWindow {
                seq,
                base: self.checkpoint.window_base,
                end: self.checkpoint.window_end,
            });
        }
        if path.leaf != entry.entry_hash {
            return Err(CheckpointVerifyError::ProofEntryMismatch {
                seq,
                leaf: path.leaf.clone(),
                entry_hash: entry.entry_hash.clone(),
            });
        }
        // The entry must BE the entry it claims to be. The leaf is a hash, and a hash copied from the
        // real entry onto an edited one still matches -- so the hash is re-derived from the op here.
        // Without this, a rewritten payload would carry a valid inclusion proof for the window.
        if !entry.hash_is_self_consistent() {
            return Err(CheckpointVerifyError::EntryHashMismatch {
                seq,
                stored: entry.entry_hash.clone(),
                computed: entry.expected_hash(),
            });
        }
        merkle::merkle_verify(path, &self.checkpoint.merkle_root)
            .map_err(|e| CheckpointVerifyError::Inclusion { seq, source: e })
    }
}

/// Why a checkpoint did not verify. Every variant is a refusal to trust it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointVerifyError {
    /// The carried index does not hash to the carried `index_hash`.
    IndexHashMismatch {
        window_end: u64,
        published: String,
        computed: String,
    },
    /// The signature does not verify under the carried public key: forged, or signed with a key that
    /// is not the node's.
    BadSignature { window_end: u64 },
    /// The public key is not decodable, so there is nothing to verify against.
    BadPublicKey { window_end: u64 },
    /// The entry a proof names is not inside this checkpoint's window.
    OutsideWindow { seq: u64, base: u64, end: u64 },
    /// The proof's leaf is not the entry it was offered for.
    ProofEntryMismatch {
        seq: u64,
        leaf: String,
        entry_hash: String,
    },
    /// The entry's stored hash is not the hash of its own op: the op was rewritten under a copied
    /// hash, so no proof about it means anything.
    EntryHashMismatch {
        seq: u64,
        stored: String,
        computed: String,
    },
    /// The inclusion path does not recompute to `merkle_root`.
    Inclusion { seq: u64, source: MerkleVerifyError },
}

impl std::fmt::Display for CheckpointVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckpointVerifyError::IndexHashMismatch {
                window_end,
                published,
                computed,
            } => write!(
                f,
                "checkpoint at {window_end} carries index hashing to {computed}, names {published}"
            ),
            CheckpointVerifyError::BadSignature { window_end } => write!(
                f,
                "checkpoint at {window_end} carries a signature that does not verify"
            ),
            CheckpointVerifyError::BadPublicKey { window_end } => write!(
                f,
                "checkpoint at {window_end} carries an undecodable public key"
            ),
            CheckpointVerifyError::OutsideWindow { seq, base, end } => write!(
                f,
                "seq {seq} is outside checkpoint window [{base}, {end})"
            ),
            CheckpointVerifyError::ProofEntryMismatch {
                seq,
                leaf,
                entry_hash,
            } => write!(
                f,
                "proof at seq {seq} commits to {leaf}, entry carries {entry_hash}"
            ),
            CheckpointVerifyError::EntryHashMismatch {
                seq,
                stored,
                computed,
            } => write!(
                f,
                "entry at seq {seq} carries hash {stored}, its op computes {computed}"
            ),
            CheckpointVerifyError::Inclusion { seq, source } => {
                write!(f, "inclusion proof at seq {seq} rejected: {source}")
            }
        }
    }
}

impl std::error::Error for CheckpointVerifyError {}
