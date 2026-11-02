//! The append-only constraint (whitepaper 2.4/2.6) and log availability (4.1), as artefacts a third
//! party can check.
//!
//! 2.4: "Signed root hashes may then be compared over a gossip network to ensure equivalent
//! histories; if divergent histories have been published, the signed roots can be used as proofs
//! that the append-only constraint has been violated." 2.6 adds that a monitor "must watch for
//! violations of the append-only constraint", separately from contract breaches.
//!
//! What existed here covered one shape of that and only one: [`crate::fraud::Equivocation`] catches
//! TWO ENTRIES AT ONE SEQ, both signed. The executor's other way to break append-only is to publish
//! two different histories at two different lengths -- which no amount of per-entry checking sees,
//! because every entry in both histories is individually well-formed and individually signed. Only a
//! statement about the log AT A LENGTH can catch it, and the only such statements this crate makes
//! are signed checkpoints. [`ForkedHistory`] is two of them, and [`adjudicate_fork`] is the check.
//!
//! 4.1 is the other half, and it is the one a dishonest executor reaches for first: "Monitors require
//! access to the full histories of a contract's oplogs and index log to perform validation. If some
//! part of history is not made available, then validation can not be accomplished. If an executor can
//! not or will not produce the full histories of logs, it may be necessary to regard the executor as
//! untrustworthy." A checkpoint is a promise that the entries it covers EXIST; [`check_availability`]
//! is what compares that promise against what the executor actually serves. Compaction is legal, but
//! only against a checkpoint that attests the cut -- which is why the attestation is required here
//! rather than optional.
//!
//! Neither of these is an economic mechanism and neither needs one: both answer "what happened",
//! never "what it costs". Bonds, slashing and challenge windows are product decisions and are
//! deliberately absent, exactly as 4.4 leaves them out of the paper.

use crate::audit::Verdict;
use crate::checkpoint::{SignedCheckpoint, TrustedCheckpoint};
use crate::contract::Contract;
use crate::oplog::OpLog;

/// Two signed statements by one executor about one contract that cannot both be true.
///
/// Both artefacts are things a monitor can be handed by anybody -- they are the node's own
/// attestations, gossiped or read off disk -- so neither side of the accusation has to be asked
/// anything, and neither has to have been watching when either was minted.
#[derive(Clone, Debug)]
pub struct ForkedHistory {
    /// One attestation.
    pub a: SignedCheckpoint,
    /// The other. Order is irrelevant; the adjudication orders them itself.
    pub b: SignedCheckpoint,
}

/// What a monitor claims about what the executor is SERVING, against what it has ATTESTED.
#[derive(Clone, Debug)]
pub struct Availability {
    /// A signed checkpoint: the executor's own claim that entries `[window_base, window_end)` exist.
    pub checkpoint: SignedCheckpoint,
    /// The checkpoint that licenses the log to start later than `checkpoint.window_base`, when the
    /// executor has compacted. Required exactly when it has: an unattested cut is indistinguishable
    /// from history being withheld.
    pub base_attestation: Option<SignedCheckpoint>,
}

/// Why the served history does not live up to what the executor attested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Withheld {
    /// The artefact does not hold together, so it settles nothing. Never a vindication: a malformed
    /// accusation is not evidence of an honest executor.
    Inadmissible { reason: String },
    /// The executor attests entries past the end of what it serves: it signed a window it will not
    /// produce. This is 4.1's "some part of history is not made available", proven rather than
    /// reported.
    TailTruncated {
        attested_end: u64,
        served_end: u64,
    },
    /// The served log starts after the attested window begins, and nothing attests the cut.
    UnattestedBase { base: u64, attested_base: u64 },
    /// A base attestation was supplied but it does not describe this log's cut: either it ends
    /// somewhere other than the log's base, or the head it attests is not the head the log's first
    /// retained entry links to.
    BaseAttestationMismatch {
        base: u64,
        attested_end: u64,
        attested_head: String,
        log_base_prev: String,
    },
}

impl std::fmt::Display for Withheld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Withheld::Inadmissible { reason } => write!(f, "inadmissible: {reason}"),
            Withheld::TailTruncated {
                attested_end,
                served_end,
            } => write!(
                f,
                "withheld: executor attests [.., {attested_end}) but serves only up to {served_end}"
            ),
            Withheld::UnattestedBase { base, attested_base } => write!(
                f,
                "withheld: log starts at {base}, past the attested window base {attested_base}, \
                 and no checkpoint attests the cut"
            ),
            Withheld::BaseAttestationMismatch {
                base,
                attested_end,
                attested_head,
                log_base_prev,
            } => write!(
                f,
                "withheld: base attestation ends at {attested_end} (log base {base}) and attests \
                 head {attested_head}, the log's first retained entry links to {log_base_prev}"
            ),
        }
    }
}

impl std::error::Error for Withheld {}

/// Settle an accusation that the executor published two divergent histories.
///
/// The check is deliberately restricted to what is DECIDABLE FROM THE TWO ARTEFACTS ALONE, because
/// that is the whole point of an append-only violation proof: a verifier holding two signed
/// statements settles it without fetching entries, without asking the executor, and without trusting
/// the accuser. Three relations are decidable that way:
///
///   * the two windows are the SAME RANGE and disagree about any signed field -- two versions of one
///     length, which is the textbook violation;
///   * the two windows share a BASE and disagree about `prev_head` -- they cannot both be the
///     continuation of one history;
///   * one window ENDS where the other BEGINS and the first's `log_head` is not the second's
///     `prev_head` -- the two do not chain, so one of them was rewritten.
///
/// Two windows with no overlap and no adjacency settle nothing about each other, and that is
/// [`Verdict::Inadmissible`] rather than [`Verdict::Honest`]: absence of a detectable fork in an
/// unrelated pair is not a vindication of either.
///
/// As everywhere else here, no pinned identity means no verdict that names a publisher, so `None`
/// returns `Inadmissible` rather than degrading to the structural check -- which any keypair
/// satisfies.
pub fn adjudicate_fork(
    contract: &Contract,
    f: &ForkedHistory,
    trusted_public_key_hex: Option<&str>,
) -> Verdict {
    let Some(trusted_public_key_hex) = trusted_public_key_hex else {
        return Verdict::Inadmissible {
            reason: "no trusted identity was supplied: an append-only violation cannot be pinned \
                     to a publisher"
                .to_string(),
        };
    };
    let trust = Some(TrustedCheckpoint {
        public_key_hex: trusted_public_key_hex,
        contract_id: &contract.id,
        contract_version: contract.version,
    });
    let (a, b) = (&f.a, &f.b);
    if let Err(e) = a.verify(trust.as_ref()) {
        return Verdict::Inadmissible {
            reason: format!("first checkpoint does not verify: {e}"),
        };
    }
    if let Err(e) = b.verify(trust.as_ref()) {
        return Verdict::Inadmissible {
            reason: format!("second checkpoint does not verify: {e}"),
        };
    }
    let (ca, cb) = (&a.checkpoint, &b.checkpoint);

    // Same range, two accounts of it. Every signed field is compared because a fork can be as small
    // as one index key: it is the same violation whether the executor rewrote the entries, the state
    // they produce, or the head they reach.
    if ca.window_base == cb.window_base && ca.window_end == cb.window_end {
        for (field, x, y) in [
            ("prev_head", &ca.prev_head, &cb.prev_head),
            ("log_head", &ca.log_head, &cb.log_head),
            ("merkle_root", &ca.merkle_root, &cb.merkle_root),
            ("index_hash", &ca.index_hash, &cb.index_hash),
        ] {
            if x != y {
                return Verdict::Dishonest {
                    seq: ca.window_end,
                    published: format!("{} at window [.., {}): {}", field, ca.window_end, x),
                    recomputed: format!("{} at the same window: {}", field, y),
                };
            }
        }
        return Verdict::Honest {
            seq: ca.window_end,
            index_hash: ca.index_hash.clone(),
        };
    }

    // Shared base, different lengths: both claim to continue one history from one point.
    if ca.window_base == cb.window_base && ca.prev_head != cb.prev_head {
        return Verdict::Dishonest {
            seq: ca.window_end.max(cb.window_end),
            published: format!("window based at {} continues from {}", ca.window_base, ca.prev_head),
            recomputed: format!("the same base continues from {}", cb.prev_head),
        };
    }

    // Adjacent: one ends exactly where the other begins, so the first's head IS the second's prev.
    if ca.window_end == cb.window_base && ca.log_head != cb.prev_head {
        return Verdict::Dishonest {
            seq: cb.window_end,
            published: format!("window [.., {}) reaches head {}", ca.window_end, ca.log_head),
            recomputed: format!("the window that follows it starts from {}", cb.prev_head),
        };
    }
    if cb.window_end == ca.window_base && cb.log_head != ca.prev_head {
        return Verdict::Dishonest {
            seq: ca.window_end,
            published: format!("window [.., {}) reaches head {}", cb.window_end, cb.log_head),
            recomputed: format!("the window that follows it starts from {}", ca.prev_head),
        };
    }

    Verdict::Inadmissible {
        reason: format!(
            "the two windows [{}, {}) and [{}, {}) neither overlap nor abut: nothing about \
             append-only is decidable from them alone",
            ca.window_base, ca.window_end, cb.window_base, cb.window_end
        ),
    }
}

/// Whether what the executor SERVES covers what it has ATTESTED (whitepaper 4.1).
///
/// The executor's own checkpoint is the claim: entries `[window_base, window_end)` exist and commit
/// to `merkle_root`. `log` is what a monitor actually holds after syncing. The question is whether
/// the second covers the first, and the answer is `Ok(())` only when it does.
///
/// A log that starts later than the checkpoint's base is NOT automatically withholding -- this crate
/// compacts, and compaction is what keeps a log from growing without bound -- but compaction is only
/// licensed by a checkpoint that attests the cut. So `base_attestation` is required exactly when
/// `log.base() > checkpoint.window_base`, and it has to describe THIS cut: its `window_end` is the
/// log's base and its `log_head` is the head the first retained entry links to. An unlicensed cut is
/// `Withheld::UnattestedBase`, which is 4.1's "it may be necessary to regard the executor as
/// untrustworthy" stated as a check rather than an opinion.
pub fn check_availability(
    contract: &Contract,
    a: &Availability,
    log: &OpLog,
    trusted_public_key_hex: Option<&str>,
) -> Result<(), Withheld> {
    let trust = trusted_public_key_hex.map(|k| TrustedCheckpoint {
        public_key_hex: k,
        contract_id: &contract.id,
        contract_version: contract.version,
    });
    a.checkpoint
        .verify(trust.as_ref())
        .map_err(|e| Withheld::Inadmissible {
            reason: format!("checkpoint does not verify: {e}"),
        })?;
    let cp = &a.checkpoint.checkpoint;
    let served_base = log.base();
    let served_end = log.next_seq();

    if served_base > cp.window_base {
        let Some(att) = &a.base_attestation else {
            return Err(Withheld::UnattestedBase {
                base: served_base,
                attested_base: cp.window_base,
            });
        };
        att.verify(trust.as_ref()).map_err(|e| Withheld::Inadmissible {
            reason: format!("base attestation does not verify: {e}"),
        })?;
        // The attestation has to be OF THIS CUT: ending at the log's base, and attesting the head the
        // first retained entry links to. A genuine checkpoint for some other window is a validly
        // signed artefact that licenses nothing here.
        if att.checkpoint.window_end != served_base || att.checkpoint.log_head != log.base_prev() {
            return Err(Withheld::BaseAttestationMismatch {
                base: served_base,
                attested_end: att.checkpoint.window_end,
                attested_head: att.checkpoint.log_head.clone(),
                log_base_prev: log.base_prev().to_string(),
            });
        }
    }

    // The tail: the executor signed a window it does not serve. Compaction only ever removes from the
    // FRONT (it is licensed by a checkpoint at the new base), so anything missing at the tail is
    // withheld, never reclaimed.
    if served_end < cp.window_end {
        return Err(Withheld::TailTruncated {
            attested_end: cp.window_end,
            served_end,
        });
    }
    Ok(())
}
