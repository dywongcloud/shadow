//! Tencent CCN private-backbone path preference for Tencent-to-Tencent peers.
//!
//! Pure classification + topology logic, kept separate from dial mechanics
//! (`PeerPool`'s private-candidate table in `lib.rs` is the integration
//! point). Deliberately conservative: a private candidate is used only when
//! BOTH sides are explicitly configured as `provider = tencent` AND their
//! region pair is explicitly listed in `HIVE_CCN_REGIONS` (or they're the
//! same region) AND the address itself passes [`is_safe_private_candidate`].
//! Nothing here infers CCN reachability from the mere shape of an IP address
//! — this repo already has one incident (`dht::relay_and_public_ip_filter`,
//! AGENTS.md's "Mesh watchdogs" / seed-filter OOM) from trusting an
//! RFC1918-shaped address as dialable without topology proof, so this module
//! never repeats that mistake: unconfigured pairs fall through to the
//! existing public/relay path, exactly as before.

use std::net::IpAddr;

/// `provider` string this module recognizes for CCN eligibility. Any other
/// value (including `None`) is never CCN-eligible — a non-Tencent peer must
/// behave exactly as it did before this module existed.
pub const PROVIDER_TENCENT: &str = "tencent";

/// Two regions considered privately connected over Tencent CCN, order-independent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RegionPair(String, String);

impl RegionPair {
    fn new(a: &str, b: &str) -> Self {
        if a <= b {
            RegionPair(a.to_string(), b.to_string())
        } else {
            RegionPair(b.to_string(), a.to_string())
        }
    }
}

/// The fleet's configured CCN region-pair topology. Built once from
/// `HIVE_CCN_REGIONS` (comma-separated `region-a:region-b` pairs, e.g.
/// `hongkong:shanghai,hongkong:singapore,singapore:tokyo`) — generic across
/// however many Tencent regions the fleet actually has, never hardcoded to a
/// specific pair. Unset/empty ⇒ no cross-region pair is CCN-eligible (a node
/// is still always CCN-eligible with itself, i.e. same-region peers), which
/// is the safe default: a fleet that never configures this env var gets
/// byte-identical behavior to before this feature existed.
#[derive(Clone, Debug, Default)]
pub struct CcnTopology {
    pairs: std::collections::HashSet<RegionPair>,
}

impl CcnTopology {
    /// Parse from a raw `HIVE_CCN_REGIONS`-shaped string. Never panics on
    /// malformed input — a bad entry is skipped, never fails the whole parse
    /// (same "loud but non-fatal" discipline as `relay_map_from_env`).
    pub fn parse(raw: &str) -> Self {
        let mut pairs = std::collections::HashSet::new();
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if let Some((a, b)) = entry.split_once(':') {
                let (a, b) = (a.trim(), b.trim());
                if !a.is_empty() && !b.is_empty() {
                    pairs.insert(RegionPair::new(a, b));
                }
            }
        }
        CcnTopology { pairs }
    }

    /// Read `HIVE_CCN_REGIONS` from the environment. `None` (unset) is
    /// distinct from `Some(empty)` only in intent, not behavior — both yield
    /// an empty topology where only same-region peers are eligible.
    pub fn from_env() -> Self {
        std::env::var("HIVE_CCN_REGIONS")
            .map(|raw| Self::parse(&raw))
            .unwrap_or_default()
    }

    /// Same region is always privately eligible (a node's own region is
    /// trivially "CCN-connected to itself" — no cross-region CCN hop needed).
    /// Otherwise the pair must be explicitly listed.
    pub fn regions_connected(&self, a: &str, b: &str) -> bool {
        a == b || self.pairs.contains(&RegionPair::new(a, b))
    }
}

/// Conservative safety gate on the private address ITSELF — defense in
/// depth even though the only caller is this node's own trusted gossip
/// merge, never raw peer input taken at face value. Mirrors
/// `dht::is_publicly_routable`'s "when in doubt, NO" posture, inverted for
/// the private case: a private-path candidate must be a genuine unicast
/// RFC1918/ULA-shaped address, never loopback/link-local/multicast/
/// unspecified/broadcast — rejecting exactly the classes a malicious or
/// misconfigured peer could use to make us dial ourselves, a metadata
/// service (169.254.169.254 is link-local, already covered), or garbage.
pub fn is_safe_private_candidate(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation())
        }
        IpAddr::V6(v6) => {
            // Matches the V4 arm's intent exactly: a private-path candidate
            // must be a genuine ULA (`fc00::/7`, RFC 4193) address, the IPv6
            // analogue of RFC1918 — not merely "not loopback/multicast/
            // unspecified", which would also accept a fully public global-
            // unicast address and let a peer register that as a "private"
            // candidate. `Ipv6Addr::is_unique_local` is still unstable, so
            // the range is spelled out directly: the top 7 bits of the first
            // segment being `1111_110` is exactly `fc00::/7`.
            !v6.is_loopback()
                && !v6.is_multicast()
                && !v6.is_unspecified()
                && (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// The end-to-end eligibility check the spec's `is_private_path_candidate`
/// asks for: both sides must declare `provider = tencent`, their regions
/// must be CCN-connected per topology, and the candidate address itself must
/// pass the safety gate. Any missing piece (no provider, no region match,
/// unsafe address) falls through to `false` — never a private dial attempt.
#[allow(clippy::too_many_arguments)]
pub fn is_private_path_candidate(
    local_provider: Option<&str>,
    local_region: &str,
    remote_provider: Option<&str>,
    remote_region: &str,
    remote_private_addr: Option<IpAddr>,
    topology: &CcnTopology,
) -> bool {
    let Some(remote_ip) = remote_private_addr else {
        return false;
    };
    local_provider == Some(PROVIDER_TENCENT)
        && remote_provider == Some(PROVIDER_TENCENT)
        && topology.regions_connected(local_region, remote_region)
        && is_safe_private_candidate(remote_ip)
}

/// Classifies which path a connection is using, purely from the remote
/// socket address the QUIC connection is actually bound to — used for the
/// observability requirement (log/trace `path=ccn_private|public_direct`).
/// A relay-routed connection has no meaningful remote *socket* address in
/// this sense; callers distinguish that case separately (relay vs direct is
/// already known from `Connection::remote_addr`'s own type in iroh).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    CcnPrivate,
    PublicDirect,
    Relay,
}

impl PathKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PathKind::CcnPrivate => "ccn_private",
            PathKind::PublicDirect => "public_direct",
            PathKind::Relay => "relay",
        }
    }
}

/// Classify an established connection's remote address against the private
/// candidate this pool attempted for that peer (if any). `private_hint` is
/// the address `PeerPool` tried to prepend for this peer, from its
/// `set_private_candidate` table.
pub fn classify_remote_addr(remote: IpAddr, private_hint: Option<IpAddr>) -> PathKind {
    match private_hint {
        Some(hint) if hint == remote => PathKind::CcnPrivate,
        _ => PathKind::PublicDirect,
    }
}
