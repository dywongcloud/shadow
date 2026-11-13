//! Inclusion proofs: what a client or a monitor hands to somebody who was not watching.
//!
//! Whitepaper 2.7: "Every message produced on the append-only log can produce an 'inclusion proof'
//! which is comprised of the log's public key, the message's sequence number, a hash of the log's
//! state at the length of the message, and a signature on the hash. Using this information it's
//! possible to demonstrate to third parties that the message was published by the log." And 3.4:
//! the participant's transaction flow returns "the oplog root proof" and, once the ack lands, "the
//! index root proof at the seq of the ack" -- so a client can "demonstrate the existence of a
//! transaction to third parties".
//!
//! Every one of those four components already existed here, in pieces that were never joined into a
//! thing a caller could hold:
//!   * the log's public key -- [`crate::signing::CheckpointSigner::public_key`], carried in every
//!     signed artefact;
//!   * the sequence number -- [`crate::oplog::Op::seq`];
//!   * the hash of the log's state at the length of the message -- [`crate::oplog::LogEntry::entry_hash`],
//!     the chain head the entry produces, which is what `ack.log_head` records;
//!   * a signature on the hash -- the per-entry Ed25519 signature over [`crate::persist::entry_sig_preimage`],
//!     which folds the ack in as well.
//!
//! What was missing was the artefact and the check: nothing produced one, so a participant was told
//! "committed" and held no way to prove it to anyone else, and a monitor's breach notice carried no
//! signature at all (see [`crate::audit::Dispute`]). This module is the join.
//!
//! The window half is optional on purpose. A checkpoint is only minted when a window closes, so an
//! entry in the open tail has no signed root over it yet -- but it does have the executor's signature
//! over its own hash, which is the inclusion proof the whitepaper describes. `window` is the extra
//! step that ties the entry to a signed Merkle root once one exists, and it is the half a verifier
//! uses when it wants the O(log n) path rather than a chain walk.
//!
//! Two limits, both the whitepaper's own, are recorded rather than papered over:
//!   * "Note however that 'freshness' is not indicated by inclusion proofs" -- an unchanging proof
//!     says the entry was published, never that it is still the head. A verifier that needs currency
//!     asks for the current head as well.
//!   * A proof is only as good as the identity it is checked against. `None` for the trusted key
//!     still runs the structural checks, and the caller then decides for itself whether it knows
//!     the key -- the same shape [`crate::checkpoint::SignedCheckpoint::verify`] uses.

use crate::checkpoint::{CheckpointVerifyError, SignedCheckpoint, TrustedCheckpoint};
use crate::contract::Contract;
use crate::fraud::{sign_entry, SignedEntry};
use crate::merkle::{merkle_prove, MerklePath};
use crate::oplog::LogEntry;
use crate::persist::entry_sig_preimage;
use crate::signing::{verify_ed25519, CheckpointSigner};

/// The signed window commitment one entry sits in: the checkpoint that attests the window, plus the
/// entry's path to the root that checkpoint signs.
///
/// The root is signed and the path is recomputed by the verifier, so neither has to be taken on
/// trust, and the path's own leaf count is checked against the window the checkpoint names -- a path
/// cut against some other number of leaves is not a proof about this window.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WindowProof {
    pub checkpoint: SignedCheckpoint,
    pub path: MerklePath,
}

impl WindowProof {
    /// Cut the path for `seq` out of a window's entry hashes, in seq order.
    ///
    /// `None` when the checkpoint does not cover `seq`: a path can only be cut against the window
    /// the root was actually taken over, and a checkpoint for another range is not evidence here no
    /// matter how valid it is.
    pub fn cut(checkpoint: SignedCheckpoint, entry_hashes: &[String], seq: u64) -> Option<WindowProof> {
        let base = checkpoint.checkpoint.window_base;
        if !checkpoint.checkpoint.covers(seq) {
            return None;
        }
        let index = seq.checked_sub(base)?;
        if index as usize >= entry_hashes.len() {
            return None;
        }
        let path = merkle_prove(entry_hashes, index).ok()?;
        Some(WindowProof { checkpoint, path })
    }
}

/// Everything a third party needs to accept one entry as published by this log: the entry, the
/// executor's signature over it, and -- once a window has closed -- its path to a signed root.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct InclusionProof {
    /// The contract whose log this is. Also inside the signed op, so it is checked against the
    /// contract the verifier is auditing rather than asserted by the artefact.
    pub contract_id: String,
    /// The published entry and the executor's signature over its hash and its ack.
    pub signed: SignedEntry,
    /// The signed window commitment covering this entry, absent while the window is still open.
    #[serde(default)]
    pub window: Option<WindowProof>,
}

/// Why a proof did not verify. Every variant is a refusal to accept the entry as published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InclusionError {
    /// The entry names a contract the verifier is not auditing. A genuine proof for one contract is
    /// a validly-signed artefact, and accepting it as another contract's history is how a foreign
    /// log gets read as this one's.
    ContractMismatch {
        published: String,
        expected: String,
    },
    /// The entry's stored hash is not the hash its own op and predecessor produce: the entry was
    /// rewritten, so nothing its signature covers describes the entry being offered.
    EntryHashMismatch {
        seq: u64,
        stored: String,
        computed: String,
    },
    /// The signature does not verify under the key the artefact carries: it was not published by
    /// whoever owns that key.
    BadSignature { seq: u64 },
    /// The signature is undecodable, so there is nothing to verify against.
    MalformedSignature { seq: u64 },
    /// The signature verifies, but under an identity that is not the one this contract's log is
    /// pinned to. A correctly-signed artefact from a key nobody pinned is a forgery with a valid
    /// signature, which is exactly the shape a forged history takes.
    UntrustedSigner { seq: u64, public_key: String },
    /// The optional window half did not verify.
    Window {
        seq: u64,
        source: CheckpointVerifyError,
    },
}

impl std::fmt::Display for InclusionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InclusionError::ContractMismatch { published, expected } => write!(
                f,
                "proof is for contract {published}, not {expected}"
            ),
            InclusionError::EntryHashMismatch {
                seq,
                stored,
                computed,
            } => write!(
                f,
                "entry at seq {seq} carries hash {stored}, its op and predecessor compute {computed}"
            ),
            InclusionError::BadSignature { seq } => write!(
                f,
                "entry at seq {seq} does not carry a signature verifying under the key it names"
            ),
            InclusionError::MalformedSignature { seq } => write!(
                f,
                "entry at seq {seq} carries a signature that is not hex"
            ),
            InclusionError::UntrustedSigner { seq, public_key } => write!(
                f,
                "entry at seq {seq} was published by {public_key}, which is not the trusted identity"
            ),
            InclusionError::Window { seq, source } => {
                write!(f, "window proof at seq {seq} rejected: {source}")
            }
        }
    }
}

impl std::error::Error for InclusionError {}

impl InclusionProof {
    /// The sequence number the message occupies (whitepaper 2.7, component 2).
    pub fn seq(&self) -> u64 {
        self.signed.entry.op.seq
    }

    /// The hash of the log's state at the length of this message (2.7, component 3): the chain head
    /// this entry produces, which is also the `log_head` its own ack records.
    ///
    /// This is the "oplog root proof" of the participant's transaction flow (3.4). It is a chain
    /// head rather than a Merkle root because the log is hash-chained; the Merkle root over a closed
    /// window is the additional commitment in [`InclusionProof::window`], and both commit to the
    /// same entries.
    pub fn log_head_at(&self) -> &str {
        &self.signed.entry.entry_hash
    }

    /// The index root the executor published at this seq (3.4, "the index root proof at the seq of
    /// the ack"). Signed with the entry, because the ack is inside the signature's preimage.
    pub fn index_root(&self) -> &str {
        &self.signed.entry.ack.index_hash
    }

    /// Whether the entry the proof describes is one the contract admits at all: a successful op of
    /// an undeclared kind is not a transaction the contract can attest, whatever its signature says.
    pub fn kind_is_declared(&self, contract: &Contract) -> bool {
        contract.spec(&self.signed.entry.op.kind).is_some()
    }

    /// Verify the whole artefact with no trust in whoever produced it.
    ///
    /// `trusted_public_key_hex` `None` runs the structural checks only -- the entry is what its own
    /// contents make it and the signature verifies under the key it carries -- and leaves "do I know
    /// this key?" to the caller. `Some(..)` adds the one binding the artefact cannot make for
    /// itself.
    ///
    /// A note on freshness, because it is the whitepaper's own caveat and not something this check
    /// can supply: a proof that verifies says the entry WAS published. It does not say the log still
    /// ends there. A verifier that needs currency compares [`InclusionProof::log_head_at`] against a
    /// head the executor is signing now.
    pub fn verify(
        &self,
        contract: &Contract,
        trusted_public_key_hex: Option<&str>,
    ) -> Result<(), InclusionError> {
        let entry = &self.signed.entry;
        let seq = entry.op.seq;
        // The artefact's own top-level contract id is a CLAIM too. It used to be documented as
        // checked and never read: a proof serialized with a foreign contract_id still returned
        // Ok(()), so the field bound nothing. Check it against the contract being audited.
        if self.contract_id != contract.id {
            return Err(InclusionError::ContractMismatch {
                published: self.contract_id.clone(),
                expected: contract.id.clone(),
            });
        }
        if entry.op.contract != contract.id {
            return Err(InclusionError::ContractMismatch {
                published: entry.op.contract.clone(),
                expected: contract.id.clone(),
            });
        }
        // The hash is re-derived rather than trusted: a hash copied from a real entry onto an edited
        // one still matches the copy, and every proof cut for it would then "verify".
        if !entry.hash_is_self_consistent() {
            return Err(InclusionError::EntryHashMismatch {
                seq,
                stored: entry.entry_hash.clone(),
                computed: entry.expected_hash(),
            });
        }
        let pk = hex::decode(&self.signed.public_key_hex)
            .map_err(|_| InclusionError::MalformedSignature { seq })?;
        let sig = hex::decode(&self.signed.sig)
            .map_err(|_| InclusionError::MalformedSignature { seq })?;
        // The preimage folds the ack in, so this is a signature over "this entry, with this account
        // of what it did" -- not merely over the bytes of the operation.
        if !verify_ed25519(&pk, &entry_sig_preimage(&entry.entry_hash, &entry.ack), &sig) {
            return Err(InclusionError::BadSignature { seq });
        }
        if let Some(trusted) = trusted_public_key_hex {
            if !self.signed.public_key_hex.eq_ignore_ascii_case(trusted) {
                return Err(InclusionError::UntrustedSigner {
                    seq,
                    public_key: self.signed.public_key_hex.clone(),
                });
            }
        }
        if let Some(w) = &self.window {
            // Under an UNPINNED verification the checkpoint's own contract binding is skipped by
            // SignedCheckpoint::verify(None). Bind it here so a foreign contract's signed window
            // cannot be attached even in the structural-only mode.
            if w.checkpoint.checkpoint.contract_id != contract.id {
                return Err(InclusionError::ContractMismatch {
                    published: w.checkpoint.checkpoint.contract_id.clone(),
                    expected: contract.id.clone(),
                });
            }
            let trust = trusted_public_key_hex.map(|k| TrustedCheckpoint {
                public_key_hex: k,
                contract_id: &contract.id,
                contract_version: contract.version,
            });
            // The checkpoint carries the signed root; verify_inclusion recomputes the path to it and
            // re-checks that the entry is the entry at that position in that window.
            w.checkpoint
                .verify(trust.as_ref())
                .map_err(|e| InclusionError::Window { seq, source: e })?;
            w.checkpoint
                .verify_inclusion(entry, &w.path)
                .map_err(|e| InclusionError::Window { seq, source: e })?;
        }
        Ok(())
    }

    /// Attach the window half, once a checkpoint covering this entry exists.
    ///
    /// Refused rather than silently ignored when the checkpoint does not cover the entry: a proof
    /// carrying a window that does not contain it would fail [`InclusionProof::verify`] for a reason
    /// that has nothing to do with the entry, and an artefact that cannot verify is worse than one
    /// that says less.
    pub fn attach_window(&mut self, w: WindowProof) -> Result<(), InclusionError> {
        let seq = self.seq();
        if !w.checkpoint.checkpoint.covers(seq) {
            return Err(InclusionError::Window {
                seq,
                source: CheckpointVerifyError::OutsideWindow {
                    seq,
                    base: w.checkpoint.checkpoint.window_base,
                    end: w.checkpoint.checkpoint.window_end,
                },
            });
        }
        self.window = Some(w);
        Ok(())
    }
}

/// Produce the proof for one entry, signed now by this node's identity.
///
/// `window` is [`WindowProof::cut`] when a checkpoint already covers the entry, `None` while the
/// entry sits in the open tail. Signing is deterministic Ed25519 over a preimage the store already
/// signed at append, so this reproduces the stored signature rather than minting a second, different
/// one for the same entry -- a proof and the log agree by construction, not by convention.
pub fn prove(
    contract: &Contract,
    entry: &LogEntry,
    signer: &dyn CheckpointSigner,
    window: Option<WindowProof>,
) -> Result<InclusionProof, String> {
    if entry.op.contract != contract.id {
        return Err(format!(
            "entry names contract {}, not {}",
            entry.op.contract, contract.id
        ));
    }
    let signed = sign_entry(entry, signer)?;
    Ok(InclusionProof {
        contract_id: contract.id.clone(),
        signed,
        window,
    })
}
