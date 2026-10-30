//! Independent-vantage public DNS: one resolver never decides a platform-wide question.
//!
//! A name the platform SERVES and a name the public INTERNET resolves are the same fact only when
//! more than one resolver says so. Any single public resolver can answer from a cache that has not
//! caught up with a delegation, from one regional view, or not answer at all — and a decision taken
//! on that one reading is wrong in one of two directions:
//!
//! * a SAN silently DROPPED from an order for a name we do serve: `www.{domain}` answers a
//!   certificate error on a hostname the operator's own DNS serves ([`super::acme::effective_sans`]
//!   — the defect this module exists for, witnessed on `geniee.site`);
//! * a SAN ADDED for a name we do not serve: Let's Encrypt fails the WHOLE order for every
//!   identifier in it, so a tenant who pointed only their apex gets no certificate at all, and
//!   every retry spends the account's failed-validation and duplicate-certificate budgets.
//!
//! The two costs are not symmetric, and the rule follows the expensive one: a dropped SAN is a
//! user-visible TLS error on one hostname that heals by itself on the next pass (300 s by default)
//! as soon as the resolvers agree; an added-but-unserved SAN takes the apex certificate down with
//! it and burns rate limits shared by every tenant on the platform. So [`Tally::admits`] requires a
//! MAJORITY of the answering vantages, never fewer than [`MIN_CONFIRMING_VANTAGES`] — a bare
//! majority, not unanimity, which is what makes one lagging cache unable to veto. That is the
//! fleet's existing standard stated once (`dns_probe::MIN_ATTESTER_REGIONS`,
//! `validate_nameservers`: never trust a single vantage, never self-attest).
//!
//! A vantage that does not answer ABSTAINS. An abstention is never a "no": counting a transport
//! error, timeout or brown-out as a vote against turns one resolver's bad minute into a silent
//! fleet-wide decision.
//!
//! This module is deliberately free of crate and third-party dependencies: the rule above is pure
//! arithmetic over observations, so it can be compiled and exercised on its own.

/// One public resolver endpoint.
///
/// `operator` is the identity a quorum counts. Two endpoints of the SAME operator are one anycast
/// cache asked twice, so they collapse into one vote ([`operator_vote`]); counting them separately
/// would let a single operator cast two votes.
pub struct Vantage {
    pub operator: &'static str,
    /// Human-readable endpoint name, for logs only.
    pub label: &'static str,
    pub endpoint: Endpoint,
}

/// How one vantage is asked.
pub enum Endpoint {
    /// JSON DoH (`?name=&type=`), the API the previous single-resolver call sites used.
    Https(&'static str),
    /// Plain DNS over UDP/53: the ONLY transport several independent operators answer on, and the
    /// one the fleet's own `dns_probe` already speaks.
    Udp(&'static str),
}

/// The vantages a platform-wide DNS question is asked from: five independent operators, no two
/// sharing a cache, and two independent transports so one egress path failing is an abstention
/// rather than a decision.
///
/// The fleet's own nameservers are deliberately ABSENT. They are authoritative for a delegated
/// zone, so they answer for a name we serve regardless of what the public internet sees — and
/// Let's Encrypt validates from the public internet. Counting them would be self-attestation, the
/// exact thing `validate_nameservers` refuses.
pub const VANTAGES: &[Vantage] = &[
    Vantage {
        operator: "cloudflare",
        label: "cloudflare-doh",
        endpoint: Endpoint::Https("https://cloudflare-dns.com/dns-query"),
    },
    Vantage {
        operator: "cloudflare",
        label: "cloudflare-udp",
        endpoint: Endpoint::Udp("1.1.1.1"),
    },
    Vantage {
        operator: "google",
        label: "google-doh",
        endpoint: Endpoint::Https("https://dns.google/resolve"),
    },
    Vantage {
        operator: "google",
        label: "google-udp",
        endpoint: Endpoint::Udp("8.8.8.8"),
    },
    Vantage {
        operator: "quad9",
        label: "quad9-udp",
        endpoint: Endpoint::Udp("9.9.9.9"),
    },
    Vantage {
        operator: "opendns",
        label: "opendns-udp",
        endpoint: Endpoint::Udp("208.67.222.222"),
    },
    Vantage {
        operator: "adguard",
        label: "adguard-udp",
        endpoint: Endpoint::Udp("94.140.14.14"),
    },
];

/// Never fewer than this many independent operators may confirm anything — two, the same floor
/// `dns_probe::MIN_ATTESTER_REGIONS` sets for nameserver attestation.
pub const MIN_CONFIRMING_VANTAGES: usize = 2;

/// What ONE vantage said about a name, before the caller's own test is applied to the values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The vantage answered; these are its records (empty = it answered and there are none).
    Records(Vec<String>),
    /// The vantage did not answer: timeout, transport error, truncated or unparsable reply, or a
    /// resolver-level failure (SERVFAIL/REFUSED). It has no opinion and ABSTAINS.
    Unavailable,
}

/// One operator's verdict on the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vote {
    Yes,
    No,
    /// No opinion: the operator's endpoints were all unavailable.
    Abstain,
}

/// Classify one vantage's answer against the caller's test (`hit`): does any of its records satisfy
/// it? An unanswered vantage abstains; an answered one with no matching record is a real `No`.
pub fn vote(answer: Answer, hit: impl Fn(&str) -> bool) -> Vote {
    match answer {
        Answer::Unavailable => Vote::Abstain,
        Answer::Records(vs) if vs.iter().any(|v| hit(v)) => Vote::Yes,
        Answer::Records(_) => Vote::No,
    }
}

/// The operators' verdicts, one per operator, ordered by operator name so the log line and the
/// decision are both deterministic.
pub fn operator_votes<'a, I, F>(answers: I, hit: F) -> Vec<(&'a str, Vote)>
where
    I: IntoIterator<Item = (&'a str, Answer)>,
    F: Fn(&str) -> bool,
{
    let mut per: std::collections::BTreeMap<&'a str, Vec<Vote>> = std::collections::BTreeMap::new();
    for (operator, answer) in answers {
        per.entry(operator).or_default().push(vote(answer, &hit));
    }
    per.into_iter()
        .map(|(operator, vs)| (operator, operator_vote(&vs)))
        .collect()
}

/// Collapse one operator's endpoints into the one vote their shared cache represents: any endpoint
/// that saw it IS the operator's answer; the operator says `No` only if an endpoint answered and
/// none of them saw it; every endpoint failing is an abstention, not a "no".
pub fn operator_vote(votes: &[Vote]) -> Vote {
    if votes.iter().any(|v| *v == Vote::Yes) {
        Vote::Yes
    } else if votes.iter().any(|v| *v == Vote::No) {
        Vote::No
    } else {
        Vote::Abstain
    }
}

/// The count that decides: operators for, against, and silent.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Tally {
    pub yes: usize,
    pub no: usize,
    pub abstain: usize,
}

impl Tally {
    pub fn note(&mut self, v: Vote) {
        match v {
            Vote::Yes => self.yes += 1,
            Vote::No => self.no += 1,
            Vote::Abstain => self.abstain += 1,
        }
    }

    /// Admit the name: a MAJORITY of the operators that answered, and never fewer than
    /// [`MIN_CONFIRMING_VANTAGES`] of them.
    ///
    /// `>` and not `>=` is the whole point: a tie is a drop. `2 yes / 2 no` means half the public
    /// internet still cannot see the delegation, and a tie broken towards adding the SAN is a
    /// failed Let's Encrypt order that takes the apex certificate with it — while a tie broken
    /// towards dropping costs one hostname for one 300 s pass and heals by itself.
    pub fn admits(&self) -> bool {
        self.yes >= MIN_CONFIRMING_VANTAGES && self.yes > self.no
    }
}

/// Fold per-operator verdicts into a [`Tally`].
pub fn tally(votes: &[(&str, Vote)]) -> Tally {
    let mut t = Tally::default();
    for (_, v) in votes {
        t.note(*v);
    }
    t
}

/// Render verdicts for a log line or a stored `last_probe` string: `google=yes cloudflare=no …`.
pub fn render(votes: &[(&str, Vote)]) -> String {
    votes
        .iter()
        .map(|(o, v)| {
            format!(
                "{o}={}",
                match v {
                    Vote::Yes => "yes",
                    Vote::No => "no",
                    Vote::Abstain => "abstain",
                }
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}
