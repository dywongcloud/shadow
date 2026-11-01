//! Fraud proofs: the compact artefact that convicts a dishonest executor.
//!
//! A signed entry is EVIDENCE, not a proof. Evidence says "somebody with the node's key published
//! this"; a fraud proof says "the executor did something it cannot have done honestly, and here is
//! the smallest thing a stranger needs to see it". The difference is everything, because evidence
//! has to be interpreted by somebody and a proof does not: a proof is checked by re-deriving a
//! number, and any two parties who check it reach the same answer.
//!
//! Two claims are provable here, and they are the two ways an executor can lie:
//!
//!   1. [`FraudProof::UnsupportedState`] -- the executor published a state the log does not
//!      support: the index hash it acked at a seq is not the hash replaying that seq produces.
//!   2. [`FraudProof::EquivocatedAcks`] -- the executor published TWO CONFLICTING ACKS for one
//!      operation: two entries at the same seq, both signed by it, that disagree.
//!
//! Both are settled by [`adjudicate_fraud`], which trusts nobody: it is handed an artefact and a
//! public key, and it recomputes. Neither side is asked anything, and neither check replays the
//! whole log -- the artefact carries one checkpoint window plus the tail after it, so the work is
//! bounded by the window, not by the history.
//!
//! What this module deliberately does NOT contain: a bond, a slash, a challenge period, or any
//! economic mechanism. Those are product decisions about what a breach COSTS, and none of them
//! change what a breach IS. This module answers the second question only.
//!
//! The soundness of the equivocation proof rests on one thing: the signature over a stored entry
//! covers its ack, not just its entry hash ([`crate::oplog::Ack::canonical`]). Without that, a
//! copy of one signed line plus a rewritten ack is a validly-signed pair, so anyone who has ever
//! seen an entry could convict an honest executor. [`SignedEntry::verify`] is what keeps a forged
//! accusation out.

use crate::audit::{adjudicate, replay_from, Dispute, Verdict};
use crate::checkpoint::{SignedCheckpoint, TrustedCheckpoint};
use crate::contract::Contract;
use crate::exec::{apply, index_hash, Index};
use crate::oplog::{Ack, LogEntry, OpLog};
use crate::persist::entry_sig_preimage;
use crate::signing::verify_ed25519;
use serde::{Deserialize, Serialize};

/// One entry as the executor published it: the entry, the signature over it, and WHOSE key made
/// that signature.
///
/// The public key travels with the artefact on purpose. A signature is only checkable against the
/// key that produced it, and a verifier that has to be told the key out of band can be told the
/// wrong one -- at which point a forged history verifies. Carrying it does not make the artefact
/// self-attesting: [`adjudicate_fraud`] still pins it to the identity the verifier expects.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedEntry {
    pub entry: LogEntry,
    /// Hex Ed25519 signature over [`entry_sig_preimage`] of this entry's hash and ack.
    pub sig: String,
    /// Hex Ed25519 public key of the publisher.
    pub public_key_hex: String,
}

impl SignedEntry {
    /// Whether the signature is the publisher's, over THIS entry and THIS ack.
    ///
    /// The ack is inside the preimage, so an entry whose ack was edited after it was signed fails
    /// here: a copy of a real line carrying a conflicting ack is not evidence of equivocation, it
    /// is a forgery, and it settles nothing.
    pub fn verify(&self) -> bool {
        let (pk, sig) = match (hex::decode(&self.public_key_hex), hex::decode(&self.sig)) {
            (Ok(pk), Ok(sig)) if !pk.is_empty() && !sig.is_empty() => (pk, sig),
            _ => return false,
        };
        let preimage = entry_sig_preimage(&self.entry.entry_hash, &self.entry.ack);
        verify_ed25519(&pk, &preimage, &sig)
    }
}

/// Sign one entry as the executor publishes it.
pub fn sign_entry<S: crate::signing::CheckpointSigner + ?Sized>(
    entry: &LogEntry,
    signer: &S,
) -> Result<SignedEntry, String> {
    let sig = signer.sign(&entry_sig_preimage(&entry.entry_hash, &entry.ack))?;
    Ok(SignedEntry {
        entry: entry.clone(),
        sig: hex::encode(sig),
        public_key_hex: hex::encode(signer.public_key()),
    })
}

/// The equivocation fraud proof: ONE seq, TWO conflicting publications, both signed by the
/// executor.
///
/// `prefix` is the entries from the checkpoint's `window_end` up to (but not including) the
/// disputed seq -- the run-up that establishes which position in the chain is under dispute. It is
/// bounded by one checkpoint window plus the tail, so settling an accusation never means replaying
/// the log from genesis.
#[derive(Clone, Debug)]
pub struct Equivocation {
    /// The last signed checkpoint at or before the disputed seq: the state both sides agree on.
    pub checkpoint: SignedCheckpoint,
    /// The entries between the checkpoint and the disputed seq.
    pub prefix: Vec<LogEntry>,
    /// One publication for the disputed seq.
    pub a: SignedEntry,
    /// The other publication for the SAME seq.
    pub b: SignedEntry,
}

/// A compact, self-contained accusation against an executor.
#[derive(Clone, Debug)]
pub enum FraudProof {
    /// The executor published two conflicting acks for one operation.
    EquivocatedAcks(Equivocation),
    /// The executor published a state the log does not support.
    UnsupportedState(Dispute),
}

impl FraudProof {
    /// What kind of breach this artefact alleges.
    pub fn kind(&self) -> &'static str {
        match self {
            FraudProof::EquivocatedAcks(_) => "equivocated_acks",
            FraudProof::UnsupportedState(_) => "unsupported_state",
        }
    }

    /// The seq under dispute, whichever kind this is.
    pub fn seq(&self) -> u64 {
        match self {
            FraudProof::EquivocatedAcks(e) => e.a.entry.op.seq,
            FraudProof::UnsupportedState(d) => d.disputed_seq,
        }
    }
}

/// Whether two publications for one seq actually say different things.
///
/// `ts_ms` is deliberately NOT part of the comparison: it is the clock reading the executor parks
/// in the ack, not a claim about state. Two entries that differ only in `ts_ms` are two readings of
/// one clock, not two versions of one history, and an accusation built on that settles nothing.
fn conflict(a: &LogEntry, b: &LogEntry) -> bool {
    if a.op.canonical() != b.op.canonical() {
        return true;
    }
    a.ack.success != b.ack.success
        || a.ack.index_hash != b.ack.index_hash
        || a.ack.error != b.ack.error
}

/// The index hash an honest executor would have published for `entry` at `base`.
///
/// A refused op changes nothing, so its honest index hash is the one it started from; a successful
/// op is the one `apply` produces. Same rule [`crate::audit::replay_from`] checks, applied to one
/// candidate.
fn expected_index_hash(contract: &Contract, base: &Index, entry: &LogEntry) -> Result<String, String> {
    if !entry.ack.success {
        return Ok(index_hash(base));
    }
    if contract.spec(&entry.op.kind).is_none() {
        return Err(format!(
            "successful op of undeclared kind {}",
            entry.op.kind
        ));
    }
    let mut index = base.clone();
    for m in apply(&index, &entry.op) {
        match m {
            crate::exec::Mutation::Put { key, value } => {
                index.insert(key, value);
            }
            crate::exec::Mutation::Delete { key } => {
                index.remove(&key);
            }
        }
    }
    Ok(index_hash(&index))
}

/// Settle one [`Equivocation`] against a known signing identity.
///
/// The third party checks, in order, that: the starting state is a checkpoint this contract's
/// identity signed; both candidates were published by ONE identity and it is the trusted one; each
/// entry is what its own contents make it and carries that publisher's signature over its ack; the
/// two entries claim the SAME position in the chain; the prefix actually reaches that position; and
/// the two disagree. Only then does it replay -- one window, not the whole log -- to say which
/// claim the log supports.
///
/// Every failure is [`Verdict::Inadmissible`], never [`Verdict::Honest`]: an artefact that does not
/// hold together settles nothing, and a malformed accusation is not a vindication.
pub fn adjudicate_equivocation(
    contract: &Contract,
    eq: &Equivocation,
    trusted_public_key_hex: Option<&str>,
) -> Verdict {
    // A verdict of Dishonest names a publisher, so an adjudication handed no trusted identity has
    // nobody to convict. Passing `None` used to fall through to the structural check alone, which
    // ANY keypair satisfies: a stranger could mint a checkpoint and two entries, hand them in, and
    // walk away with a `Dishonest` verdict against a node that signed none of it -- a proof that
    // convicts the innocent. Settling nothing is the only honest answer here, so this is
    // Inadmissible rather than a weaker-but-passing check.
    let Some(trusted_public_key_hex) = trusted_public_key_hex else {
        return Verdict::Inadmissible {
            reason: "no trusted identity was supplied: a verdict of dishonesty cannot be pinned to \
                     a publisher"
                .to_string(),
        };
    };
    let trust = Some(TrustedCheckpoint {
        public_key_hex: trusted_public_key_hex,
        contract_id: &contract.id,
        contract_version: contract.version,
    });
    if let Err(e) = eq.checkpoint.verify(trust.as_ref()) {
        return Verdict::Inadmissible {
            reason: format!("checkpoint does not verify: {e}"),
        };
    }
    // One publisher, and the one we expect. Two different keys means the accuser stitched together
    // entries from two histories; an unexpected key means nobody claimed to be this executor.
    if eq.a.public_key_hex != eq.b.public_key_hex {
        return Verdict::Inadmissible {
            reason: format!(
                "the two entries name different publishers ({} and {})",
                eq.a.public_key_hex, eq.b.public_key_hex
            ),
        };
    }
    if eq.a.public_key_hex != trusted_public_key_hex {
        return Verdict::Inadmissible {
            reason: format!(
                "entries were published by {}, which is not the trusted identity",
                eq.a.public_key_hex
            ),
        };
    }
    for (label, cand) in [("first", &eq.a), ("second", &eq.b)] {
        // A copied entry_hash under a rewritten op convicts nobody: the entry has to be what its
        // own contents make it before its signature means anything.
        if !cand.entry.hash_is_self_consistent() {
            return Verdict::Inadmissible {
                reason: format!("{label} entry carries a hash its own op does not produce"),
            };
        }
        if !cand.verify() {
            return Verdict::Inadmissible {
                reason: format!(
                    "{label} entry does not carry the publisher's signature over its ack"
                ),
            };
        }
    }
    let seq = eq.a.entry.op.seq;
    if seq != eq.b.entry.op.seq {
        return Verdict::Inadmissible {
            reason: format!(
                "the two entries are at different seqs ({} and {})",
                seq, eq.b.entry.op.seq
            ),
        };
    }
    if eq.a.entry.prev_hash != eq.b.entry.prev_hash {
        return Verdict::Inadmissible {
            reason: "the two entries link to different predecessors".to_string(),
        };
    }
    // The run-up has to land exactly where both candidates say they are. Without this, two real
    // entries from two different positions could be offered as an equivocation.
    let cp = &eq.checkpoint.checkpoint;
    let prefix_log = match OpLog::restore(cp.window_end, &cp.log_head, eq.prefix.clone()) {
        Ok(l) => l,
        Err(e) => {
            return Verdict::Inadmissible {
                reason: format!("prefix chain is broken at seq {}: {}", e.seq, e.stored),
            }
        }
    };
    if prefix_log.head() != eq.a.entry.prev_hash {
        return Verdict::Inadmissible {
            reason: format!(
                "the prefix reaches {}, both entries claim to follow {}",
                prefix_log.head(),
                eq.a.entry.prev_hash
            ),
        };
    }
    if !conflict(&eq.a.entry, &eq.b.entry) {
        return Verdict::Inadmissible {
            reason: "the two entries agree: there is no equivocation to adjudicate".to_string(),
        };
    }
    let base = match replay_from(contract, &prefix_log, Some(cp)) {
        Ok(i) => i,
        Err(e) => {
            return Verdict::Inadmissible {
                reason: format!("replay could not be completed: {e}"),
            }
        }
    };
    // Report the candidate the log contradicts, or the first one when neither is supported.
    for cand in [&eq.a, &eq.b] {
        match expected_index_hash(contract, &base, &cand.entry) {
            Err(e) => {
                return Verdict::Dishonest {
                    seq,
                    published: cand.entry.ack.index_hash.clone(),
                    recomputed: e,
                }
            }
            Ok(expected) => {
                if expected != cand.entry.ack.index_hash {
                    return Verdict::Dishonest {
                        seq,
                        published: cand.entry.ack.index_hash.clone(),
                        recomputed: expected,
                    };
                }
            }
        }
    }
    // Both claims are individually reproducible -- they disagree about something else (two
    // operations at one seq, or the same op acked twice with different success/error). That is
    // still equivocation: the executor published two versions of one seq and at most one of them
    // can be what the log holds. Name the pair; the disagreement is the breach.
    Verdict::Dishonest {
        seq,
        published: format!("{}", ack_digest(&eq.a.entry.ack)),
        recomputed: format!("{}", ack_digest(&eq.b.entry.ack)),
    }
}

/// A short, stable name for one ack's claim, so a verdict can name two acks that differ in
/// `success` or `error` rather than in the index hash.
fn ack_digest(a: &Ack) -> String {
    let mut out = String::with_capacity(80);
    out.push_str(if a.success { "ok" } else { "failed" });
    out.push('/');
    out.push_str(&a.index_hash);
    if let Some(e) = &a.error {
        out.push('/');
        out.push_str(e);
    }
    out
}

/// Settle any [`FraudProof`] against a known signing identity.
///
/// One entry point so a verifier cannot accidentally apply the wrong check to an artefact: the
/// kind of the proof picks the adjudication, and both paths return the same [`Verdict`].
pub fn adjudicate_fraud(
    contract: &Contract,
    proof: &FraudProof,
    trusted_public_key_hex: Option<&str>,
) -> Verdict {
    match proof {
        FraudProof::EquivocatedAcks(e) => {
            adjudicate_equivocation(contract, e, trusted_public_key_hex)
        }
        FraudProof::UnsupportedState(d) => adjudicate(contract, d, trusted_public_key_hex),
    }
}
