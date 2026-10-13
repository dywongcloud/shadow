//! Tencent CCN private-backbone path preference for Tencent-to-Tencent peers: pure classification
//! + topology, used only when BOTH sides declare `provider = tencent`, their region pair is listed in
//! `HIVE_CCN_REGIONS` (or equal) AND the address passes [`is_safe_private_candidate`]. Reachability
//! is never inferred from an IP's shape; unconfigured pairs use the public/relay path.

use std::net::IpAddr;

/// The `provider` value eligible for CCN; anything else (including `None`) never is.
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

/// The fleet's CCN region-pair topology from `HIVE_CCN_REGIONS` (`a:b,...`); unset ⇒ only
/// same-region peers are eligible, i.e. behaviour identical to before this feature.
#[derive(Clone, Debug, Default)]
pub struct CcnTopology {
    pairs: std::collections::HashSet<RegionPair>,
}

impl CcnTopology {
    /// Parse a `HIVE_CCN_REGIONS`-shaped string; a bad entry is skipped, never fails the parse.
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

    /// Read `HIVE_CCN_REGIONS`; unset and empty both yield the same-region-only topology.
    pub fn from_env() -> Self {
        std::env::var("HIVE_CCN_REGIONS")
            .map(|raw| Self::parse(&raw))
            .unwrap_or_default()
    }

    /// Same region is always eligible; otherwise the pair must be listed.
    pub fn regions_connected(&self, a: &str, b: &str) -> bool {
        a == b || self.pairs.contains(&RegionPair::new(a, b))
    }
}

/// Safety gate on the private address itself (defense in depth, mirroring
/// `dht::is_publicly_routable`'s "when in doubt, NO"): a genuine unicast RFC1918/ULA address,
/// never loopback/link-local/multicast/unspecified/broadcast.
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
            !v6.is_loopback()
                && !v6.is_multicast()
                && !v6.is_unspecified()
                && (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// End-to-end eligibility: both sides `provider = tencent`, regions CCN-connected per topology,
/// and the candidate passes the safety gate; any missing piece is `false`.
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

/// Which path a connection uses, from the remote socket address the QUIC connection is bound to
/// (relay-routed connections are distinguished separately by the caller).
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

/// Classify an established connection's remote address against the private candidate `PeerPool`
/// tried for that peer, if any.
pub fn classify_remote_addr(remote: IpAddr, private_hint: Option<IpAddr>) -> PathKind {
    match private_hint {
        Some(hint) if hint == remote => PathKind::CcnPrivate,
        _ => PathKind::PublicDirect,
    }
}
