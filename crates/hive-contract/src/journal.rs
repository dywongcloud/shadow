//! Balanced double-entry journal for the billing ledger.
//!
//! CONCEPTS ONLY: this module reimplements the ideas of a double-entry general ledger (balanced
//! debits/credits, a chart of accounts, append-only reversal-only corrections, idempotency keys,
//! a BLAKE3 hash chain) for this platform. No code from any BUSL-1.1 project is vendored.
//!
//! Where the state lives: the ONE canonical platform state is still `state.json` + `store_sync`.
//! This journal's canonical state is the signed, node-local contract oplog under
//! `$HIVE_DATA/contract-oplog`, sharded by contract id. It is rebuilt by replaying `journal.entry`
//! ops through [`Journal::post`], so the SAME engine-level invariants gate an original write and a
//! replay. Nothing here adds a field to `PlatformSnapshot`, `LedgerEntry`, `BillingAccount` or the
//! two-element `billing.snapshot()` tuple: a mixed-version fleet cannot lose state to this module.
//!
//! What this is NOT: an authorization system, a consensus protocol, a bond/incentive model or a
//! freshness guarantee. Tenancy is decided by the platform seam before an op is built. An inclusion
//! proof says an entry WAS published. This journal is NODE-LOCAL evidence, never fleet-wide.
//!
//! Hashing: BLAKE3 mints every NEW content identity (`fluid_core::content_hash`). Canonical bytes
//! are LENGTH-PREFIXED `<len>:<bytes>` through `oplog::canonical_fields`, never a bare join.
//! Ed25519 signing reuses the platform's existing `CheckpointSigner`; no new key is minted here.

use crate::checkpoint::{Checkpoint, SignedCheckpoint};
use crate::exec::index_hash;
use crate::merkle::{merkle_prove, merkle_root, merkle_verify, MerklePath};
use crate::oplog::{canonical_fields, LogEntry, Op};
use crate::signing::{verify_ed25519, CheckpointSigner};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// New content identities minted by this module. Distinct domains so a journal leaf can never be
/// confused with a WORM audit leaf or a contract oplog entry.
pub const JOURNAL_DOMAIN: &[u8] = b"hive-contract/journal/entry/v1";
pub const WORM_DOMAIN: &[u8] = b"hive-contract/journal/worm/v1";
pub const COMMITMENT_DOMAIN: &[u8] = b"hive-contract/journal/commitment/v1";
pub const RECORD_DOMAIN: &[u8] = b"hive-contract/journal/record/v1";
pub const GENESIS: &str = "hive-contract/journal/genesis/v1";

/// This platform bills in USD minor units (cents). No float or decimal path exists.
pub const CURRENCY_USD: &str = "USD";
/// Mirrors `billing::MAX_AMOUNT_CENTS`: a `u64 above i64::MAX` cast to `i64` goes NEGATIVE.
pub const MAX_LINE_CENTS: i64 = 1_000_000_000_000;
/// Above this size, ONE actor is not enough to post an entry (four-eyes control).
pub const FOUR_EYES_THRESHOLD_CENTS: i64 = 100_000_000;

pub const JOURNAL_ENTRY: &str = "journal.entry";
pub const JOURNAL_REFUSAL: &str = "journal.refusal";
pub const JOURNAL_RECONCILIATION: &str = "journal.reconciliation";
pub const JOURNAL_RETENTION: &str = "journal.retention";
pub const JOURNAL_CHECKPOINT: &str = "journal.checkpoint";

/// Closed payload schemas, shared by the contract declaration and the payload builder. One source
/// of truth: a hand-built payload with a missing or unexpected key is refused by `validate`.
pub const JOURNAL_ENTRY_KEYS: &[&str] = &[
    "tenant_tag", "entry_seq", "entry_kind", "currency", "idempotency_key", "actor",
    "second_actor", "posted_ms", "memo", "reverses_seq", "lines", "content_hash",
    "prev_hash", "chain_hash",
];
pub const JOURNAL_REFUSAL_KEYS: &[&str] = &[
    "tenant_tag", "refusal_id", "at_ms", "invariant_code", "detail", "idempotency_key",
    "writes_attempted",
];
pub const JOURNAL_RECON_KEYS: &[&str] = &[
    "tenant_tag", "run_id", "at_ms", "from_seq", "to_seq", "entries_scanned",
    "discrepancy_count", "chain_tip", "worm_tip", "record_hash",
];
pub const JOURNAL_RETENTION_KEYS: &[&str] = &[
    "tenant_tag", "event_id", "at_ms", "from_seq", "to_seq", "entries_retained",
    "entries_pruned", "decision", "audit_tip",
];
pub const JOURNAL_CHECKPOINT_KEYS: &[&str] = &[
    "tenant_tag", "from_seq", "to_seq", "entry_count", "merkle_root", "journal_tip",
    "worm_tip", "signature_hex", "public_key_hex", "signer_id", "contract_id",
];

/// Every invariant violation is a typed refusal. No panic, no silent skip, no policy-only gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalError {
    TooFewLines { lines: usize },
    ZeroAmount,
    AmountOutOfRange { cents: i64 },
    UnbalancedEntry { debits: i128, credits: i128 },
    UnknownAccount(String),
    AccountClosed(String),
    CurrencyMismatch { expected: String, found: String },
    CurrencyPrecision(String),
    NegativeBalance { account: String, balance: i128 },
    BalanceOverflow { account: String },
    ExposureLimitExceeded { account: String, balance: i128, limit: i128 },
    LegalHold { account: String },
    FourEyesRequired { cents: i128 },
    ReversalTargetNotPosted { seq: u64 },
    AlreadyReversed { seq: u64 },
    ReversalLinesMismatch { seq: u64 },
    CorrectionWithoutReversal { seq: u64 },
    SequenceViolation { expected: u64, found: u64 },
    ChainBreak { seq: u64 },
    IdempotencyConflict { key: String, original_seq: u64 },
    SuspenseNotCleared { balance: i128 },
    GlobalLedgerEquation { left: i128, right: i128 },
    MalformedLines(String),
    MissingApprover,
    BadRange { from: u64, to: u64 },
    Refusal(String),
}

impl JournalError {
    /// Stable machine-readable control code for the WORM chain and the evidence report.
    pub fn code(&self) -> &'static str {
        match self {
            JournalError::TooFewLines { .. } => "TooFewLines",
            JournalError::ZeroAmount => "ZeroAmount",
            JournalError::AmountOutOfRange { .. } => "AmountOutOfRange",
            JournalError::UnbalancedEntry { .. } => "UnbalancedEntry",
            JournalError::UnknownAccount(_) => "UnknownAccount",
            JournalError::AccountClosed(_) => "AccountClosed",
            JournalError::CurrencyMismatch { .. } => "CurrencyMismatch",
            JournalError::CurrencyPrecision(_) => "CurrencyPrecision",
            JournalError::NegativeBalance { .. } => "NegativeBalance",
            JournalError::BalanceOverflow { .. } => "BalanceOverflow",
            JournalError::ExposureLimitExceeded { .. } => "ExposureLimitExceeded",
            JournalError::LegalHold { .. } => "LegalHold",
            JournalError::FourEyesRequired { .. } => "FourEyesRequired",
            JournalError::ReversalTargetNotPosted { .. } => "ReversalTargetNotPosted",
            JournalError::AlreadyReversed { .. } => "AlreadyReversed",
            JournalError::ReversalLinesMismatch { .. } => "ReversalLinesMismatch",
            JournalError::CorrectionWithoutReversal { .. } => "CorrectionWithoutReversal",
            JournalError::SequenceViolation { .. } => "SequenceViolation",
            JournalError::ChainBreak { .. } => "ChainBreak",
            JournalError::IdempotencyConflict { .. } => "IdempotencyConflict",
            JournalError::SuspenseNotCleared { .. } => "SuspenseNotCleared",
            JournalError::GlobalLedgerEquation { .. } => "GlobalLedgerEquation",
            JournalError::MalformedLines(_) => "MalformedLines",
            JournalError::MissingApprover => "MissingApprover",
            JournalError::BadRange { .. } => "BadRange",
            JournalError::Refusal(_) => "Refusal",
        }
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::TooFewLines { lines } => write!(f, "entry has {lines} line(s); >=2 required"),
            JournalError::ZeroAmount => f.write_str("a zero amount is not representable"),
            JournalError::AmountOutOfRange { cents } => write!(f, "amount {cents} is out of range"),
            JournalError::UnbalancedEntry { debits, credits } => {
                write!(f, "debits {debits} != credits {credits}")
            }
            JournalError::UnknownAccount(a) => write!(f, "account {a} is not in the chart"),
            JournalError::AccountClosed(a) => write!(f, "account {a} is closed"),
            JournalError::CurrencyMismatch { expected, found } => {
                write!(f, "currency {found} does not match {expected}")
            }
            JournalError::CurrencyPrecision(s) => write!(f, "currency precision: {s}"),
            JournalError::NegativeBalance { account, balance } => {
                write!(f, "{account} would go negative at {balance}")
            }
            JournalError::BalanceOverflow { account } => write!(f, "{account} balance overflowed"),
            JournalError::ExposureLimitExceeded { account, balance, limit } => {
                write!(f, "{account} exposure {balance} exceeds {limit}")
            }
            JournalError::LegalHold { account } => write!(f, "account {account} is under legal hold"),
            JournalError::FourEyesRequired { cents } => {
                write!(f, "two distinct approvers required for {cents} cents")
            }
            JournalError::ReversalTargetNotPosted { seq } => {
                write!(f, "entry {seq} was not a posted original")
            }
            JournalError::AlreadyReversed { seq } => write!(f, "entry {seq} already reversed"),
            JournalError::ReversalLinesMismatch { seq } => {
                write!(f, "reversal of {seq} must flip its lines exactly")
            }
            JournalError::CorrectionWithoutReversal { seq } => {
                write!(f, "entry {seq} has no posted reversal")
            }
            JournalError::SequenceViolation { expected, found } => {
                write!(f, "journal sequence expected {expected}, found {found}")
            }
            JournalError::ChainBreak { seq } => write!(f, "journal hash chain breaks at {seq}"),
            JournalError::IdempotencyConflict { key, original_seq } => {
                write!(f, "idempotency key {key} already posted at {original_seq}")
            }
            JournalError::SuspenseNotCleared { balance } => {
                write!(f, "suspense account is not cleared ({balance})")
            }
            JournalError::GlobalLedgerEquation { left, right } => {
                write!(f, "global ledger equation {left} != {right}")
            }
            JournalError::MalformedLines(d) => write!(f, "malformed journal lines: {d}"),
            JournalError::MissingApprover => f.write_str("primary approver is missing"),
            JournalError::BadRange { from, to } => write!(f, "range {from}..{to} is not admissible"),
            JournalError::Refusal(d) => write!(f, "refusal: {d}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// A non-zero integer amount in minor units. The private field is the type-level guarantee, but
/// `Deserialize` bypasses it (exactly as `Contract`'s public fields bypass `declare`), so EVERY
/// write rechecks through [`Amount::validated`]. No float, no decimal, no `expect` panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amount(i64);

impl Amount {
    pub fn new(minor_units: i64) -> Result<Amount, JournalError> {
        if minor_units == 0 {
            return Err(JournalError::ZeroAmount);
        }
        if minor_units < 0 || minor_units > MAX_LINE_CENTS {
            return Err(JournalError::AmountOutOfRange { cents: minor_units });
        }
        Ok(Amount(minor_units))
    }

    /// Recheck a value that arrived through `Deserialize`, a stored record or a replay.
    pub fn validated(self) -> Result<Amount, JournalError> {
        Amount::new(self.0)
    }

    pub fn minor_units(self) -> i64 {
        self.0
    }

    pub fn as_i128(self) -> i128 {
        self.0 as i128
    }

    /// Parse a MINOR-UNIT string. A decimal point, exponent, sign or whitespace is refused: this
    /// platform has no fractional-cent path, and `u64 as i64` above the ceiling goes negative.
    pub fn parse_minor_units(s: &str) -> Result<Amount, JournalError> {
        if s.is_empty()
            || !s.bytes().all(|b| b.is_ascii_digit())
            || s.chars().any(|c| matches!(c, '.' | ',' | 'e' | 'E' | '-' | '+'))
        {
            return Err(JournalError::CurrencyPrecision(format!(
                "{s} is not an integer number of minor units"
            )));
        }
        let n: i64 = s
            .parse()
            .map_err(|_| JournalError::CurrencyPrecision(format!("{s} is out of i64 range")))?;
        Amount::new(n)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Debit,
    Credit,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Debit => "Debit",
            Direction::Credit => "Credit",
        }
    }

    pub fn parse(s: &str) -> Result<Direction, JournalError> {
        match s {
            "Debit" => Ok(Direction::Debit),
            "Credit" => Ok(Direction::Credit),
            other => Err(JournalError::MalformedLines(format!("bad direction {other}"))),
        }
    }

    pub fn flip(self) -> Direction {
        match self {
            Direction::Debit => Direction::Credit,
            Direction::Credit => Direction::Debit,
        }
    }

    /// Signed contribution to a RAW debit-minus-credit balance.
    pub fn sign(self) -> i128 {
        match self {
            Direction::Debit => 1,
            Direction::Credit => -1,
        }
    }
}

/// Asset/Expense increase by Debit; Liability/Equity/Income increase by Credit. Suspense is
/// neutral and MUST be cleared for the Global Ledger Equation to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountType {
    Asset,
    Liability,
    Equity,
    Income,
    Expense,
    Suspense,
}

impl AccountType {
    pub fn normal_direction(self) -> Direction {
        match self {
            AccountType::Asset | AccountType::Expense | AccountType::Suspense => Direction::Debit,
            AccountType::Liability | AccountType::Equity | AccountType::Income => Direction::Credit,
        }
    }

    pub fn is_neutral(self) -> bool {
        self == AccountType::Suspense
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AccountType::Asset => "Asset",
            AccountType::Liability => "Liability",
            AccountType::Equity => "Equity",
            AccountType::Income => "Income",
            AccountType::Expense => "Expense",
            AccountType::Suspense => "Suspense",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountStatus {
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub code: String,
    pub kind: AccountType,
    pub currency: String,
    pub status: AccountStatus,
    pub exposure_limit_cents: Option<i64>,
    pub legal_hold: bool,
    pub four_eyes: bool,
}

impl Account {
    pub fn open(code: &str, kind: AccountType) -> Account {
        Account {
            code: code.to_string(),
            kind,
            currency: CURRENCY_USD.to_string(),
            status: AccountStatus::Open,
            exposure_limit_cents: None,
            legal_hold: false,
            four_eyes: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalLine {
    pub account: String,
    pub direction: Direction,
    pub amount: Amount,
    pub currency: String,
    /// A BLAKE3 digest, never operator free text: the signed log must not carry a secret.
    pub memo: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Original,
    Reversal,
    Correction,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Original => "original",
            EntryKind::Reversal => "reversal",
            EntryKind::Correction => "correction",
        }
    }

    pub fn parse(s: &str) -> Result<EntryKind, JournalError> {
        match s {
            "original" => Ok(EntryKind::Original),
            "reversal" => Ok(EntryKind::Reversal),
            "correction" => Ok(EntryKind::Correction),
            other => Err(JournalError::MalformedLines(format!("bad entry kind {other}"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDraft {
    pub lines: Vec<JournalLine>,
    pub currency: String,
    pub idempotency_key: String,
    pub actor: String,
    pub second_actor: Option<String>,
    pub posted_ms: u64,
    /// BLAKE3 digest of a note, never the note itself.
    pub memo: String,
    pub kind: EntryKind,
    pub reverses: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub seq: u64,
    pub lines: Vec<JournalLine>,
    pub currency: String,
    pub idempotency_key: String,
    pub actor: String,
    pub second_actor: Option<String>,
    pub posted_ms: u64,
    pub memo: String,
    pub kind: EntryKind,
    pub reverses: Option<u64>,
    /// BLAKE3 over the length-prefixed canonical fields, INCLUDING every line in order.
    pub content_hash: String,
    pub prev_hash: String,
    /// BLAKE3 over `<seq>:<bytes>|<prev_hash>:<bytes>|<content_hash>:<bytes>`.
    pub chain_hash: String,
}

impl JournalEntry {
    /// Canonical bytes of the content. Line ORDER and COUNT are inside, so a rewritten, omitted,
    /// duplicated or reordered line changes this digest.
    pub fn content_bytes(&self) -> Vec<u8> {
        let fields: Vec<String> = vec![
            self.seq.to_string(),
            self.kind.as_str().to_string(),
            self.reverses.map(|s| s.to_string()).unwrap_or_default(),
            self.currency.clone(),
            self.idempotency_key.clone(),
            self.actor.clone(),
            self.second_actor.clone().unwrap_or_default(),
            self.posted_ms.to_string(),
            self.memo.clone(),
            encode_lines(&self.lines),
        ];
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        canonical_fields(&refs)
    }

    pub fn compute_content_hash(&self) -> String {
        fluid_core::content_hash(JOURNAL_DOMAIN, &self.content_bytes())
    }

    pub fn compute_chain_hash(&self) -> String {
        let seq = self.seq.to_string();
        let bytes = canonical_fields(&[&seq, &self.prev_hash, &self.content_hash]);
        fluid_core::content_hash(JOURNAL_DOMAIN, &bytes)
    }
}

/// Length-prefixed encoding of one line list. `oplog::canonical_fields` prefixes every field, so
/// `1e` between lines and `1f` between fields can never be forged by a value containing them.
pub fn encode_lines(lines: &[JournalLine]) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\x1e');
        }
        let amount = l.amount.minor_units().to_string();
        let fields = canonical_fields(&[
            &l.account,
            l.direction.as_str(),
            &amount,
            &l.currency,
            &l.memo,
        ]);
        out.push_str(&String::from_utf8_lossy(&fields));
    }
    out
}

/// Parse the length-prefixed field list of ONE line.
fn parse_fields(s: &str) -> Result<Vec<String>, JournalError> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    loop {
        let colon = bytes[i..]
            .iter()
            .position(|b| *b == b':')
            .map(|p| i + p)
            .ok_or_else(|| JournalError::MalformedLines("missing length prefix".into()))?;
        if colon == i || !bytes[i..colon].iter().all(|b| b.is_ascii_digit()) {
            return Err(JournalError::MalformedLines("bad length prefix".into()));
        }
        let n: usize = std::str::from_utf8(&bytes[i..colon])
            .map_err(|_| JournalError::MalformedLines("bad prefix bytes".into()))?
            .parse()
            .map_err(|_| JournalError::MalformedLines("prefix out of range".into()))?;
        let start = colon + 1;
        let end = start
            .checked_add(n)
            .ok_or_else(|| JournalError::MalformedLines("prefix overflow".into()))?;
        if end > bytes.len() {
            return Err(JournalError::MalformedLines("truncated field".into()));
        }
        let value = std::str::from_utf8(&bytes[start..end])
            .map_err(|_| JournalError::MalformedLines("field is not utf8".into()))?;
        out.push(value.to_string());
        i = end;
        if i == bytes.len() {
            break;
        }
        if bytes[i] != 0x1f {
            return Err(JournalError::MalformedLines("bad field separator".into()));
        }
        i += 1;
        if i >= bytes.len() {
            return Err(JournalError::MalformedLines("trailing separator".into()));
        }
    }
    Ok(out)
}

pub fn decode_lines(s: &str) -> Result<Vec<JournalLine>, JournalError> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let mut lines = Vec::new();
    for part in s.split('\x1e') {
        let f = parse_fields(part)?;
        if f.len() != 5 {
            return Err(JournalError::MalformedLines("line needs five fields".into()));
        }
        let amount = Amount::parse_minor_units(&f[2])?;
        amount.validated()?;
        lines.push(JournalLine {
            account: f[0].clone(),
            direction: Direction::parse(&f[1])?,
            amount,
            currency: f[3].clone(),
            memo: f[4].clone(),
        });
    }
    Ok(lines)
}

/// One immutable invariant-violation record. A refused write is a RECORD, never a prevention of
/// the platform mutation that already happened upstream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalRecord {
    pub at_ms: u64,
    pub invariant_code: String,
    pub detail: String,
    pub idempotency_key: String,
    pub writes_attempted: u64,
}

/// An INDEPENDENT, separately domain-separated WORM audit hash chain. Honest scope: it is a
/// SECOND tamper-evident log written by the same process with the same node identity, NOT
/// independent verification. An attacker controlling the writer rewrites both chains. There is no
/// second signing key, no external anchor, and no consensus here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WormEvent {
    pub seq: u64,
    pub prev_hash: String,
    pub hash: String,
    pub at_ms: u64,
    pub kind: String,
    pub detail: String,
}

impl WormEvent {
    pub fn compute_hash(&self) -> String {
        let seq = self.seq.to_string();
        let ts = self.at_ms.to_string();
        let bytes = canonical_fields(&[&seq, &self.prev_hash, &ts, &self.kind, &self.detail]);
        fluid_core::content_hash(WORM_DOMAIN, &bytes)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WormChain {
    #[serde(default)]
    events: Vec<WormEvent>,
    #[serde(default)]
    head: String,
    #[serde(default)]
    next_seq: u64,
}

impl WormChain {
    pub fn append(&mut self, at_ms: u64, kind: &str, detail: &str) -> WormEvent {
        let prev = if self.events.is_empty() {
            GENESIS.to_string()
        } else {
            self.head.clone()
        };
        let mut e = WormEvent {
            seq: self.next_seq,
            prev_hash: prev,
            hash: String::new(),
            at_ms,
            kind: kind.to_string(),
            detail: detail.chars().take(256).collect(),
        };
        e.hash = e.compute_hash();
        self.head = e.hash.clone();
        self.next_seq = self.next_seq.saturating_add(1);
        self.events.push(e.clone());
        e
    }

    pub fn tip(&self) -> String {
        if self.events.is_empty() {
            GENESIS.to_string()
        } else {
            self.head.clone()
        }
    }

    pub fn events(&self) -> &[WormEvent] {
        &self.events
    }

    pub fn from_events(events: Vec<WormEvent>) -> WormChain {
        let next_seq = events.last().map(|e| e.seq.saturating_add(1)).unwrap_or(0);
        let head = events.last().map(|e| e.hash.clone()).unwrap_or_default();
        WormChain {
            events,
            head,
            next_seq,
        }
    }

    /// Verify the WHOLE chain, not just the tip. A verifier receives the events in the report.
    pub fn verify_events(events: &[WormEvent]) -> Result<String, JournalError> {
        let mut prev = GENESIS.to_string();
        for (i, e) in events.iter().enumerate() {
            if e.seq != i as u64 || e.prev_hash != prev || e.hash != e.compute_hash() {
                return Err(JournalError::ChainBreak { seq: e.seq });
            }
            prev = e.hash.clone();
        }
        Ok(if events.is_empty() {
            GENESIS.to_string()
        } else {
            prev
        })
    }

    pub fn verify(&self) -> Result<String, JournalError> {
        WormChain::verify_events(&self.events)
    }
}

/// Persistent immutable reconciliation record: timestamp, range, entries scanned, discrepancy
/// count and BOTH chain tips. Its own hash makes a rewritten record detectable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationRecord {
    pub run_id: String,
    pub at_ms: u64,
    pub from_seq: u64,
    pub to_seq: u64,
    pub entries_scanned: u64,
    pub discrepancy_count: u64,
    pub chain_tip: String,
    pub worm_tip: String,
    pub record_hash: String,
}

impl ReconciliationRecord {
    pub fn compute_hash(&self) -> String {
        let bytes = canonical_fields(&[
            &self.run_id,
            &self.at_ms.to_string(),
            &self.from_seq.to_string(),
            &self.to_seq.to_string(),
            &self.entries_scanned.to_string(),
            &self.discrepancy_count.to_string(),
            &self.chain_tip,
            &self.worm_tip,
        ]);
        fluid_core::content_hash(RECORD_DOMAIN, &bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionEvent {
    pub event_id: String,
    pub at_ms: u64,
    pub from_seq: u64,
    pub to_seq: u64,
    pub entries_retained: u64,
    /// Raw `billing_ledger` rows pruned by the platform's existing retention path. The journal
    /// NEVER prunes its own lines.
    pub entries_pruned: u64,
    pub decision: String,
    pub audit_tip: String,
}

/// A consistent point-in-time read fence: the sequence and head a reader saw, never a lock held
/// across a writer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalFence {
    pub sequence: u64,
    pub head: String,
    pub entry_count: u64,
    pub worm_tip: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSnapshot {
    #[serde(default)]
    pub fence: JournalFence,
    #[serde(default)]
    pub entries: Vec<JournalEntry>,
    #[serde(default)]
    pub balances: BTreeMap<String, i64>,
}

impl Default for JournalFence {
    fn default() -> JournalFence {
        JournalFence {
            sequence: 0,
            head: GENESIS.to_string(),
            entry_count: 0,
            worm_tip: GENESIS.to_string(),
        }
    }
}

/// The ONE journal writer. Every `post` rechecks every invariant at the engine/type layer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    #[serde(default)]
    accounts: BTreeMap<String, Account>,
    #[serde(default)]
    entries: Vec<JournalEntry>,
    /// Point lookups by seq bypass a full scan: `entry(seq)` is O(log n).
    #[serde(default)]
    seq_index: BTreeMap<u64, usize>,
    #[serde(default)]
    idempotency: BTreeMap<String, u64>,
    #[serde(default)]
    balances: BTreeMap<String, i64>,
    #[serde(default)]
    reversed: BTreeMap<u64, u64>,
    #[serde(default)]
    refusals: Vec<RefusalRecord>,
    #[serde(default)]
    reconciliations: Vec<ReconciliationRecord>,
    #[serde(default)]
    retention_events: Vec<RetentionEvent>,
    #[serde(default)]
    worm: WormChain,
    #[serde(default)]
    next_seq: u64,
    #[serde(default)]
    head: String,
    #[serde(default)]
    writes_attempted: u64,
    #[serde(default)]
    writes_refused: u64,
}

impl Journal {
    /// Platform-owned chart of accounts. Six account types, no tenant-controlled path component.
    pub fn default_chart() -> Journal {
        let mut j = Journal::default();
        for (code, kind) in [
            ("cash", AccountType::Asset),
            ("accounts_receivable", AccountType::Asset),
            ("prepaid_credits", AccountType::Liability),
            ("equity", AccountType::Equity),
            ("revenue", AccountType::Income),
            ("usage_expense", AccountType::Expense),
            ("suspense", AccountType::Suspense),
        ] {
            j.accounts.insert(code.to_string(), Account::open(code, kind));
        }
        j
    }

    pub fn head(&self) -> String {
        if self.entries.is_empty() {
            GENESIS.to_string()
        } else {
            self.head.clone()
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq.saturating_add(1)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    pub fn accounts(&self) -> &BTreeMap<String, Account> {
        &self.accounts
    }

    /// O(log n) point lookup, never a capped scan.
    pub fn entry(&self, seq: u64) -> Option<&JournalEntry> {
        self.seq_index.get(&seq).and_then(|i| self.entries.get(*i))
    }

    pub fn has_idempotency(&self, key: &str) -> bool {
        self.idempotency.contains_key(key)
    }

    pub fn refusals(&self) -> &[RefusalRecord] {
        &self.refusals
    }

    pub fn reconciliations(&self) -> &[ReconciliationRecord] {
        &self.reconciliations
    }

    pub fn retention_events(&self) -> &[RetentionEvent] {
        &self.retention_events
    }

    pub fn worm(&self) -> &WormChain {
        &self.worm
    }

    pub fn writes_attempted(&self) -> u64 {
        self.writes_attempted
    }

    pub fn writes_refused(&self) -> u64 {
        self.writes_refused
    }

    pub fn set_status(&mut self, code: &str, status: AccountStatus) -> Result<(), JournalError> {
        let a = self
            .accounts
            .get_mut(code)
            .ok_or_else(|| JournalError::UnknownAccount(code.to_string()))?;
        a.status = status;
        Ok(())
    }

    pub fn set_controls(
        &mut self,
        code: &str,
        exposure_limit_cents: Option<i64>,
        legal_hold: bool,
        four_eyes: bool,
    ) -> Result<(), JournalError> {
        let a = self
            .accounts
            .get_mut(code)
            .ok_or_else(|| JournalError::UnknownAccount(code.to_string()))?;
        a.exposure_limit_cents = exposure_limit_cents;
        a.legal_hold = legal_hold;
        a.four_eyes = four_eyes;
        Ok(())
    }

    pub fn fence(&self) -> JournalFence {
        JournalFence {
            sequence: self.next_seq.saturating_sub(1),
            head: self.head(),
            entry_count: self.entries.len() as u64,
            worm_tip: self.worm.tip(),
        }
    }

    /// Consistent point-in-time read: a VALUE, not a lock. A later writer cannot change it.
    pub fn snapshot_at(&self, fence: &JournalFence) -> Result<JournalSnapshot, JournalError> {
        if fence.entry_count > self.entries.len() as u64 {
            return Err(JournalError::BadRange {
                from: 0,
                to: fence.entry_count,
            });
        }
        let entries: Vec<JournalEntry> = self
            .entries
            .iter()
            .take(fence.entry_count as usize)
            .cloned()
            .collect();
        if entries.last().map(|e| e.chain_hash.as_str()).unwrap_or(GENESIS) != fence.head {
            return Err(JournalError::ChainBreak { seq: fence.sequence });
        }
        Ok(JournalSnapshot {
            fence: fence.clone(),
            entries,
            balances: self.balances.clone(),
        })
    }

    /// All 16 invariant gates, in this order, BEFORE any entry, balance or index mutation.
    pub fn post(&mut self, draft: JournalDraft) -> Result<JournalEntry, JournalError> {
        self.writes_attempted = self.writes_attempted.saturating_add(1);
        match self.post_checked(draft.clone()) {
            Ok(entry) => {
                self.worm.append(
                    entry.posted_ms,
                    "EntryPosted",
                    &format!("seq={} hash={}", entry.seq, entry.chain_hash),
                );
                Ok(entry)
            }
            Err(e) => {
                self.writes_refused = self.writes_refused.saturating_add(1);
                let rec = RefusalRecord {
                    at_ms: draft.posted_ms,
                    invariant_code: e.code().to_string(),
                    detail: e.to_string(),
                    idempotency_key: draft.idempotency_key.clone(),
                    writes_attempted: self.writes_attempted,
                };
                self.refusals.push(rec);
                self.worm.append(
                    draft.posted_ms,
                    "InvariantViolation",
                    &format!("{} {}", e.code(), draft.idempotency_key),
                );
                Err(e)
            }
        }
    }

    fn post_checked(&mut self, draft: JournalDraft) -> Result<JournalEntry, JournalError> {
        // 1. >= 2 lines.
        if draft.lines.len() < 2 {
            return Err(JournalError::TooFewLines {
                lines: draft.lines.len(),
            });
        }
        // 2. Idempotency key duplicate detection.
        if let Some(original) = self.idempotency.get(&draft.idempotency_key) {
            return Err(JournalError::IdempotencyConflict {
                key: draft.idempotency_key.clone(),
                original_seq: *original,
            });
        }
        // 3. Non-zero amounts rechecked for a Deserialize'd Amount; bounds the i64 cast.
        for l in &draft.lines {
            l.amount.validated()?;
        }
        // 4/5/6. Account existence, status, legal hold, currency match.
        if draft.currency != CURRENCY_USD {
            return Err(JournalError::CurrencyMismatch {
                expected: CURRENCY_USD.to_string(),
                found: draft.currency.clone(),
            });
        }
        let mut four_eyes = false;
        for l in &draft.lines {
            if l.currency != draft.currency {
                return Err(JournalError::CurrencyMismatch {
                    expected: draft.currency.clone(),
                    found: l.currency.clone(),
                });
            }
            let a = self
                .accounts
                .get(&l.account)
                .ok_or_else(|| JournalError::UnknownAccount(l.account.clone()))?;
            if a.currency != draft.currency {
                return Err(JournalError::CurrencyMismatch {
                    expected: draft.currency.clone(),
                    found: a.currency.clone(),
                });
            }
            if a.status == AccountStatus::Closed {
                return Err(JournalError::AccountClosed(l.account.clone()));
            }
            if a.legal_hold {
                return Err(JournalError::LegalHold {
                    account: l.account.clone(),
                });
            }
            four_eyes |= a.four_eyes;
        }
        // 7. Debits == credits, in i128 so a long run of maximal amounts cannot wrap.
        let mut debits: i128 = 0;
        let mut credits: i128 = 0;
        for l in &draft.lines {
            match l.direction {
                Direction::Debit => debits = debits.saturating_add(l.amount.as_i128()),
                Direction::Credit => credits = credits.saturating_add(l.amount.as_i128()),
            }
        }
        if debits != credits {
            return Err(JournalError::UnbalancedEntry { debits, credits });
        }
        // 8. Four eyes on a large entry or an account that demands it.
        if four_eyes || debits >= FOUR_EYES_THRESHOLD_CENTS as i128 {
            let second = draft.second_actor.as_deref().unwrap_or("").trim();
            if second.is_empty() || second == draft.actor.trim() {
                return Err(JournalError::FourEyesRequired { cents: debits });
            }
        }
        if draft.actor.trim().is_empty() {
            return Err(JournalError::MissingApprover);
        }
        // 9/10. Reversal of a POSTED original only, exactly one reversal, exact flipped lines.
        match draft.kind {
            EntryKind::Reversal => {
                let target_seq = draft
                    .reverses
                    .ok_or(JournalError::ReversalTargetNotPosted { seq: 0 })?;
                let target = self
                    .entry(target_seq)
                    .ok_or(JournalError::ReversalTargetNotPosted { seq: target_seq })?;
                if target.kind != EntryKind::Original {
                    return Err(JournalError::ReversalTargetNotPosted { seq: target_seq });
                }
                if self.reversed.contains_key(&target_seq) {
                    return Err(JournalError::AlreadyReversed { seq: target_seq });
                }
                if encode_lines(&draft.lines) != encode_lines(&reversed_lines(target)) {
                    return Err(JournalError::ReversalLinesMismatch { seq: target_seq });
                }
            }
            EntryKind::Correction => {
                let original = draft
                    .reverses
                    .ok_or(JournalError::CorrectionWithoutReversal { seq: 0 })?;
                if !self.reversed.contains_key(&original) {
                    return Err(JournalError::CorrectionWithoutReversal { seq: original });
                }
            }
            EntryKind::Original => {}
        }
        // 11. Candidate balances, with typed refusals for Asset/Expense and exposure limits.
        let mut candidate = self.balances.clone();
        for l in &draft.lines {
            let slot = candidate.entry(l.account.clone()).or_insert(0);
            let next = (*slot as i128).saturating_add(l.direction.sign() * l.amount.as_i128());
            *slot = i64::try_from(next)
                .map_err(|_| JournalError::BalanceOverflow { account: l.account.clone() })?;
        }
        let mut left: i128 = 0;
        let mut right: i128 = 0;
        for (code, account) in &self.accounts {
            let raw = *candidate.get(code).unwrap_or(&0) as i128;
            if let Some(limit) = account.exposure_limit_cents {
                if raw.abs() > limit as i128 {
                    return Err(JournalError::ExposureLimitExceeded {
                        account: code.clone(),
                        balance: raw,
                        limit: limit as i128,
                    });
                }
            }
            if account.kind.is_neutral() {
                if raw != 0 {
                    return Err(JournalError::SuspenseNotCleared { balance: raw });
                }
                continue;
            }
            if matches!(account.kind, AccountType::Asset | AccountType::Expense) && raw < 0 {
                return Err(JournalError::NegativeBalance {
                    account: code.clone(),
                    balance: raw,
                });
            }
            match account.kind.normal_direction() {
                Direction::Debit => left = left.saturating_add(raw),
                Direction::Credit => right = right.saturating_add(-raw),
            }
        }
        // 12. Global Ledger Equation: Sigma(Assets + Expenses) == Sigma(Liabilities + Equity + Income).
        if left != right {
            return Err(JournalError::GlobalLedgerEquation { left, right });
        }
        // 13. Strictly monotonic sequence, no gaps, BLAKE3 chain H(seq || prev || content).
        let seq = self.next_seq.saturating_add(1);
        let prev_hash = self.head();
        let mut entry = JournalEntry {
            seq,
            lines: draft.lines,
            currency: draft.currency,
            idempotency_key: draft.idempotency_key,
            actor: draft.actor,
            second_actor: draft.second_actor,
            posted_ms: draft.posted_ms,
            memo: draft.memo,
            kind: draft.kind,
            reverses: draft.reverses,
            content_hash: String::new(),
            prev_hash,
            chain_hash: String::new(),
        };
        entry.content_hash = entry.compute_content_hash();
        entry.chain_hash = entry.compute_chain_hash();
        // COMMIT: every mutation lands together, after every gate passed.
        self.seq_index.insert(seq, self.entries.len());
        self.entries.push(entry.clone());
        self.idempotency.insert(entry.idempotency_key.clone(), seq);
        self.balances = candidate;
        self.next_seq = seq;
        self.head = entry.chain_hash.clone();
        if entry.kind == EntryKind::Reversal {
            if let Some(original) = entry.reverses {
                self.reversed.insert(original, seq);
            }
        }
        Ok(entry)
    }

    /// Group commit: one caller lock, one WORM batch event, EVERY entry still gated by `post`.
    pub fn post_batch(&mut self, drafts: Vec<JournalDraft>) -> Vec<Result<JournalEntry, JournalError>> {
        let outcomes: Vec<Result<JournalEntry, JournalError>> =
            drafts.into_iter().map(|d| self.post(d)).collect();
        let ok = outcomes.iter().filter(|o| o.is_ok()).count();
        let refused = outcomes.len() - ok;
        self.worm.append(
            hive_core_now_ms(),
            "GroupCommit",
            &format!("posted={ok} refused={refused}"),
        );
        outcomes
    }

    /// Append-only correction step 1: flip the original's lines at their original amounts.
    pub fn reverse(
        &mut self,
        original_seq: u64,
        idempotency_key: &str,
        actor: &str,
        second_actor: Option<&str>,
        posted_ms: u64,
    ) -> Result<JournalEntry, JournalError> {
        let target = self
            .entry(original_seq)
            .cloned()
            .ok_or(JournalError::ReversalTargetNotPosted { seq: original_seq })?;
        let draft = JournalDraft {
            lines: reversed_lines(&target),
            currency: target.currency,
            idempotency_key: idempotency_key.to_string(),
            actor: actor.to_string(),
            second_actor: second_actor.map(str::to_string),
            posted_ms,
            memo: format!("reversal-of-{original_seq}"),
            kind: EntryKind::Reversal,
            reverses: Some(original_seq),
        };
        self.post(draft)
    }

    /// Append-only correction step 2: a NEW correction entry. The original is NEVER modified.
    pub fn correct(
        &mut self,
        original_seq: u64,
        lines: Vec<JournalLine>,
        idempotency_key: &str,
        actor: &str,
        second_actor: Option<&str>,
        posted_ms: u64,
    ) -> Result<JournalEntry, JournalError> {
        let draft = JournalDraft {
            lines,
            currency: CURRENCY_USD.to_string(),
            idempotency_key: idempotency_key.to_string(),
            actor: actor.to_string(),
            second_actor: second_actor.map(str::to_string),
            posted_ms,
            memo: format!("correction-of-{original_seq}"),
            kind: EntryKind::Correction,
            reverses: Some(original_seq),
        };
        self.post(draft)
    }

    /// Recompute EVERY entry's content hash and chain hash, and the whole Global Ledger Equation.
    pub fn verify(&self) -> Result<(), JournalError> {
        let mut prev = GENESIS.to_string();
        for (i, e) in self.entries.iter().enumerate() {
            let expected = i as u64 + 1;
            if e.seq != expected {
                return Err(JournalError::SequenceViolation {
                    expected,
                    found: e.seq,
                });
            }
            if e.prev_hash != prev
                || e.content_hash != e.compute_content_hash()
                || e.chain_hash != e.compute_chain_hash()
            {
                return Err(JournalError::ChainBreak { seq: e.seq });
            }
            for l in &e.lines {
                l.amount.validated()?;
            }
            if e.lines.len() < 2 {
                return Err(JournalError::TooFewLines { lines: e.lines.len() });
            }
            prev = e.chain_hash.clone();
        }
        if prev != self.head() {
            return Err(JournalError::ChainBreak {
                seq: self.next_seq,
            });
        }
        self.worm.verify()?;
        self.global_ledger_equation()?;
        Ok(())
    }

    /// Sigma(Assets + Expenses) == Sigma(Liabilities + Equity + Income), with Suspense cleared.
    pub fn global_ledger_equation(&self) -> Result<(i128, i128), JournalError> {
        let mut left: i128 = 0;
        let mut right: i128 = 0;
        for (code, a) in &self.accounts {
            let raw = *self.balances.get(code).unwrap_or(&0) as i128;
            if a.kind.is_neutral() {
                if raw != 0 {
                    return Err(JournalError::SuspenseNotCleared { balance: raw });
                }
                continue;
            }
            match a.kind.normal_direction() {
                Direction::Debit => left = left.saturating_add(raw),
                Direction::Credit => right = right.saturating_add(-raw),
            }
        }
        if left != right {
            return Err(JournalError::GlobalLedgerEquation { left, right });
        }
        Ok((left, right))
    }

    /// Immutable reconciliation record. It is appended, never rewritten; the WORM chain names it.
    pub fn reconcile(&mut self, at_ms: u64) -> ReconciliationRecord {
        let mut discrepancies = 0u64;
        if self.verify().is_err() {
            discrepancies = discrepancies.saturating_add(1);
        }
        for r in &self.reconciliations {
            if r.record_hash != r.compute_hash() {
                discrepancies = discrepancies.saturating_add(1);
            }
        }
        let mut record = ReconciliationRecord {
            run_id: format!("recon-{}-{}-{}", at_ms, self.next_seq, discrepancies),
            at_ms,
            from_seq: self.entries.first().map(|e| e.seq).unwrap_or(0),
            to_seq: self.next_seq,
            entries_scanned: self.entries.len() as u64,
            discrepancy_count: discrepancies,
            chain_tip: self.head(),
            worm_tip: self.worm.tip(),
            record_hash: String::new(),
        };
        record.record_hash = record.compute_hash();
        self.reconciliations.push(record.clone());
        self.worm.append(
            at_ms,
            "Reconciliation",
            &format!("{} discrepancies={}", record.run_id, record.discrepancy_count),
        );
        record
    }

    /// Retention evaluation. Journal lines are NEVER pruned; raw `billing_ledger` rows are pruned
    /// by the platform's existing path. Honest residual: a row pruned BEFORE its signed oplog entry
    /// lands has no reconstructed sample. See `docs/double-entry-design.md`.
    pub fn evaluate_retention(&mut self, at_ms: u64, raw_rows_pruned: u64) -> RetentionEvent {
        let event = RetentionEvent {
            event_id: format!("retention-{at_ms}-{}", self.next_seq),
            at_ms,
            from_seq: self.entries.first().map(|e| e.seq).unwrap_or(0),
            to_seq: self.next_seq,
            entries_retained: self.entries.len() as u64,
            entries_pruned: raw_rows_pruned,
            decision: "retain-journal-lines; raw-ledger-pruning-attested-by-signed-oplog".to_string(),
            audit_tip: self.worm.tip(),
        };
        self.retention_events.push(event.clone());
        self.worm.append(
            at_ms,
            "RetentionEvaluation",
            &format!(
                "{} retained={} raw_pruned={}",
                event.event_id, event.entries_retained, event.entries_pruned
            ),
        );
        event
    }

    pub fn save_json(&self, path: &std::path::Path) -> Result<(), String> {
        let body = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_json(path: &std::path::Path) -> Result<Journal, String> {
        let body = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let j: Journal = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        j.verify().map_err(|e| e.to_string())?;
        Ok(j)
    }

    /// Rebuild the journal from the signed contract oplog. The SAME `post` gates every replayed
    /// entry, and the payload's hashes are re-derived. A missing, duplicated or reordered op is a
    /// typed refusal; a rewritten line cannot reproduce its content hash.
    pub fn replay(log: &[LogEntry]) -> Result<Journal, JournalError> {
        let mut j = Journal::default_chart();
        for e in log {
            if e.op.kind != JOURNAL_ENTRY || !e.ack.success {
                continue;
            }
            let p = &e.op.payload;
            let seq: u64 = p
                .get("entry_seq")
                .ok_or_else(|| JournalError::MalformedLines("missing entry_seq".into()))?
                .parse()
                .map_err(|_| JournalError::MalformedLines("bad entry_seq".into()))?;
            if seq != j.next_seq() {
                return Err(JournalError::SequenceViolation {
                    expected: j.next_seq(),
                    found: seq,
                });
            }
            let draft = draft_from_payload(p)?;
            let posted = j.post(draft)?;
            if posted.content_hash != *p.get("content_hash").unwrap_or(&String::new())
                || posted.chain_hash != *p.get("chain_hash").unwrap_or(&String::new())
                || posted.prev_hash != *p.get("prev_hash").unwrap_or(&String::new())
            {
                return Err(JournalError::ChainBreak { seq });
            }
        }
        j.verify()?;
        Ok(j)
    }

    /// Detect a presented journal view that disagrees with the signed oplog replay. Re-derives
    /// each line list's content hash, so a rewritten, omitted, duplicated or reordered LINE is
    /// detected even when the presented entry still carries the original chain hash.
    pub fn detect_tamper(
        log: &[LogEntry],
        presented: &[JournalEntry],
    ) -> Result<(), JournalError> {
        let replayed = Journal::replay(log)?;
        if replayed.entries.len() != presented.len() {
            return Err(JournalError::SequenceViolation {
                expected: replayed.entries.len() as u64,
                found: presented.len() as u64,
            });
        }
        for (a, b) in replayed.entries.iter().zip(presented.iter()) {
            if a.seq != b.seq
                || a.content_hash != b.compute_content_hash()
                || a.chain_hash != b.chain_hash
                || a.prev_hash != b.prev_hash
            {
                return Err(JournalError::ChainBreak { seq: a.seq });
            }
        }
        Ok(())
    }
}

fn reversed_lines(target: &JournalEntry) -> Vec<JournalLine> {
    target
        .lines
        .iter()
        .map(|l| JournalLine {
            account: l.account.clone(),
            direction: l.direction.flip(),
            amount: l.amount,
            currency: l.currency.clone(),
            memo: l.memo.clone(),
        })
        .collect()
}

fn hive_core_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn num(p: &BTreeMap<String, String>, key: &str) -> Result<u64, JournalError> {
    p.get(key)
        .ok_or_else(|| JournalError::MalformedLines(format!("missing {key}")))?
        .parse()
        .map_err(|_| JournalError::MalformedLines(format!("bad {key}")))
}

pub fn draft_from_payload(
    p: &BTreeMap<String, String>,
) -> Result<JournalDraft, JournalError> {
    let reverses = match p.get("reverses_seq").map(String::as_str) {
        Some("") | None => None,
        Some(v) => Some(
            v.parse()
                .map_err(|_| JournalError::MalformedLines("bad reverses_seq".into()))?,
        ),
    };
    let second = p.get("second_actor").cloned().unwrap_or_default();
    Ok(JournalDraft {
        lines: decode_lines(
            p.get("lines")
                .ok_or_else(|| JournalError::MalformedLines("missing lines".into()))?,
        )?,
        currency: p.get("currency").cloned().unwrap_or_default(),
        idempotency_key: p.get("idempotency_key").cloned().unwrap_or_default(),
        actor: p.get("actor").cloned().unwrap_or_default(),
        second_actor: if second.is_empty() { None } else { Some(second) },
        posted_ms: num(p, "posted_ms")?,
        memo: p.get("memo").cloned().unwrap_or_default(),
        kind: EntryKind::parse(p.get("entry_kind").map(String::as_str).unwrap_or(""))?,
        reverses,
    })
}

/// The exact, closed payload of one journal entry. Every declared key is present, no extras.
pub fn entry_payload(e: &JournalEntry, tenant_tag: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("tenant_tag".to_string(), tenant_tag.to_string()),
        ("entry_seq".to_string(), e.seq.to_string()),
        ("entry_kind".to_string(), e.kind.as_str().to_string()),
        ("currency".to_string(), e.currency.clone()),
        ("idempotency_key".to_string(), e.idempotency_key.clone()),
        ("actor".to_string(), e.actor.clone()),
        ("second_actor".to_string(), e.second_actor.clone().unwrap_or_default()),
        ("posted_ms".to_string(), e.posted_ms.to_string()),
        ("memo".to_string(), e.memo.clone()),
        (
            "reverses_seq".to_string(),
            e.reverses.map(|s| s.to_string()).unwrap_or_default(),
        ),
        ("lines".to_string(), encode_lines(&e.lines)),
        ("content_hash".to_string(), e.content_hash.clone()),
        ("prev_hash".to_string(), e.prev_hash.clone()),
        ("chain_hash".to_string(), e.chain_hash.clone()),
    ])
}

/// Canonical index value for ANY journal op: length-prefixed over its declared key order. Used by
/// `exec::apply` so replay can compare a presented row against the signed log byte for byte.
pub fn op_index_value(op: &Op) -> String {
    let keys: &[&str] = match op.kind.as_str() {
        JOURNAL_ENTRY => JOURNAL_ENTRY_KEYS,
        JOURNAL_REFUSAL => JOURNAL_REFUSAL_KEYS,
        JOURNAL_RECONCILIATION => JOURNAL_RECON_KEYS,
        JOURNAL_RETENTION => JOURNAL_RETENTION_KEYS,
        JOURNAL_CHECKPOINT => JOURNAL_CHECKPOINT_KEYS,
        _ => &[],
    };
    let values: Vec<String> = keys
        .iter()
        .map(|k| op.payload.get(*k).cloned().unwrap_or_default())
        .collect();
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    String::from_utf8_lossy(&canonical_fields(&refs)).into_owned()
}

/// Index key for a journal op. The id is a platform-generated number or digest, never a tenant
/// string used as a host path.
pub fn op_index_key(ns: &str, op: &Op) -> String {
    let id = match op.kind.as_str() {
        JOURNAL_ENTRY => op.payload.get("entry_seq").cloned().unwrap_or_default(),
        JOURNAL_REFUSAL => op.payload.get("refusal_id").cloned().unwrap_or_default(),
        JOURNAL_RECONCILIATION => op.payload.get("run_id").cloned().unwrap_or_default(),
        JOURNAL_RETENTION => op.payload.get("event_id").cloned().unwrap_or_default(),
        JOURNAL_CHECKPOINT => format!(
            "{}-{}",
            op.payload.get("from_seq").cloned().unwrap_or_default(),
            op.payload.get("to_seq").cloned().unwrap_or_default()
        ),
        _ => String::new(),
    };
    format!("{ns}/journal/{}/{}", op.kind.replace('.', "-"), crate::exec::escape_component(&id))
}

/// An Ed25519-signed Merkle commitment over ANY journal entry range. REUSES the crate's existing
/// `merkle_root` and `SignedCheckpoint::sign`; nothing new is invented. A verifier recomputes the
/// root from `entry_hashes` and checks the signature under a PINNED public key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RangeCommitment {
    pub from_seq: u64,
    pub to_seq: u64,
    pub entry_hashes: Vec<String>,
    pub checkpoint: SignedCheckpoint,
    pub sampled_path: MerklePath,
    pub signature_hex: String,
    pub public_key_hex: String,
}

pub fn range_commitment(
    j: &Journal,
    contract_id: &str,
    contract_version: u32,
    from: u64,
    to: u64,
    signer: &dyn CheckpointSigner,
    signed_ms: u64,
) -> Result<RangeCommitment, JournalError> {
    if from == 0 || to < from {
        return Err(JournalError::BadRange { from, to });
    }
    let hashes: Vec<String> = (from..=to)
        .map(|s| {
            j.entry(s)
                .map(|e| e.chain_hash.clone())
                .ok_or(JournalError::SequenceViolation {
                    expected: s,
                    found: 0,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let root = merkle_root(&hashes);
    let mut index: BTreeMap<String, String> = BTreeMap::new();
    for s in from..=to {
        if let Some(e) = j.entry(s) {
            index.insert(
                format!("journal/{s}/lines"),
                encode_lines(&e.lines),
            );
            index.insert(format!("journal/{s}/chain"), e.chain_hash.clone());
        }
    }
    let path = merkle_prove(&hashes, 0)
        .map_err(|e| JournalError::Refusal(e.to_string()))?;
    let cp = Checkpoint {
        contract_id: contract_id.to_string(),
        contract_version,
        window_base: from,
        window_end: to.saturating_add(1),
        prev_head: j
            .entry(from)
            .map(|e| e.prev_hash.clone())
            .unwrap_or_else(|| GENESIS.to_string()),
        log_head: j.entry(to).map(|e| e.chain_hash.clone()).unwrap_or_default(),
        merkle_root: root,
        index_hash: index_hash(&index),
        index,
        ts_ms: signed_ms,
    };
    let canonical = cp.canonical();
    let sig = signer
        .sign(&canonical)
        .map_err(JournalError::Refusal)?;
    let signed = SignedCheckpoint::sign(cp, signer, signed_ms)
        .map_err(|e| JournalError::Refusal(e.to_string()))?;
    Ok(RangeCommitment {
        from_seq: from,
        to_seq: to,
        entry_hashes: hashes,
        checkpoint: signed,
        sampled_path: path,
        signature_hex: hex::encode(sig),
        public_key_hex: hex::encode(signer.public_key()),
    })
}

/// Independent check of a range commitment: recompute the root, the signature, and an inclusion
/// path under a PINNED identity. No journal is consulted, so this is genuinely a second party.
pub fn verify_range_commitment(
    c: &RangeCommitment,
    contract_id: &str,
    trusted_public_key_hex: &str,
) -> Result<(), String> {
    let cp = &c.checkpoint;
    if cp.checkpoint.contract_id != contract_id {
        return Err("commitment names a foreign contract".into());
    }
    if !c.public_key_hex.eq_ignore_ascii_case(trusted_public_key_hex)
        || !cp.public_key_hex.eq_ignore_ascii_case(trusted_public_key_hex)
    {
        return Err("commitment key is not the pinned identity".into());
    }
    if cp.checkpoint.window_base != c.from_seq
        || cp.checkpoint.window_end != c.to_seq.saturating_add(1)
    {
        return Err("commitment range does not match its checkpoint".into());
    }
    let root = merkle_root(&c.entry_hashes);
    if root != cp.checkpoint.merkle_root {
        return Err("recomputed Merkle root differs from the signed root".into());
    }
    let pk = hex::decode(&c.public_key_hex).map_err(|_| "bad public key".to_string())?;
    let sig = hex::decode(&c.signature_hex).map_err(|_| "bad signature".to_string())?;
    if !verify_ed25519(&pk, &cp.checkpoint.canonical(), &sig) {
        return Err("Ed25519 signature does not verify".into());
    }
    merkle_verify(&c.sampled_path, &root).map_err(|e| e.to_string())?;
    if c.sampled_path.leaf != *c.entry_hashes.first().ok_or("empty range")? {
        return Err("sampled leaf is not the first entry of the range".into());
    }
    let trust = crate::checkpoint::TrustedCheckpoint {
        public_key_hex: trusted_public_key_hex,
        contract_id,
        contract_version: cp.checkpoint.contract_version,
    };
    cp.verify(Some(&trust)).map_err(|e| e.to_string())?;
    Ok(())
}

/// A helper used by the platform seam to build a balanced two-line draft from ONE legacy
/// single-entry ledger row. The legacy ledger stays untouched as a compatibility view.
pub fn draft_from_legacy_row(
    id: &str,
    amount_cents: i64,
    note_hash: &str,
    posted_ms: u64,
    actor: &str,
) -> Result<JournalDraft, JournalError> {
    let magnitude = amount_cents
        .checked_abs()
        .ok_or(JournalError::AmountOutOfRange { cents: amount_cents })?;
    let amount = Amount::new(magnitude)?;
    let (debit, credit) = if amount_cents < 0 {
        ("usage_expense", "prepaid_credits")
    } else {
        ("cash", "revenue")
    };
    Ok(JournalDraft {
        lines: vec![
            JournalLine {
                account: debit.to_string(),
                direction: Direction::Debit,
                amount,
                currency: CURRENCY_USD.to_string(),
                memo: note_hash.to_string(),
            },
            JournalLine {
                account: credit.to_string(),
                direction: Direction::Credit,
                amount,
                currency: CURRENCY_USD.to_string(),
                memo: note_hash.to_string(),
            },
        ],
        currency: CURRENCY_USD.to_string(),
        idempotency_key: format!("ledger:{id}"),
        actor: actor.to_string(),
        second_actor: None,
        posted_ms,
        memo: note_hash.to_string(),
        kind: EntryKind::Original,
        reverses: None,
    })
}
