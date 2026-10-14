//! The one guard for HTTP requests the node makes to a URL a tenant influences: webhook endpoints,
//! queue callbacks, SIEM sinks, Redis REST URLs, remote images, model downloads, and the platform's
//! own calls into a tenant's app. A tenant URL is untrusted input aimed at the node's network
//! position (loopback admin ports, the VPC, the cloud metadata service), so the rules live here and
//! the clients enforce them by construction.
//!
//! * [`validate_url`] is the static layer, run at registration and again before every request:
//!   https only, no userinfo, and a host that is neither an internal name nor a non-public address
//!   literal (an IP literal never reaches a resolver, so only this layer sees it).
//! * [`client`] adds the connect-time layer: every address its resolver returns is filtered through
//!   [`is_public`] at the moment the connection is made, so a name that resolved publicly at
//!   registration and rebinds to 169.254.169.254 later is refused. It never follows a redirect and
//!   never uses a proxy (a proxy resolves names itself). [`following_client`] is the same client
//!   that follows [`MAX_REDIRECTS`] redirects, each hop re-validated.
//! * [`same_origin_client`] is for a URL the PLATFORM builds toward a tenant's app (the local
//!   gateway, a deployment URL): the target is trusted, the app's answer is not, so only a redirect
//!   that stays on the same origin is followed.
//! * [`internal_client`] is for the fleet's own calls (peer admin URLs, DNS and billing providers):
//!   it never follows a redirect, because whoever answers can aim it anywhere.
//!
//! `HIVE_OUTBOUND_ALLOW_PRIVATE=1` lifts the scheme and address rules of [`validate_url`] and the
//! connect-time filter, for a developer's machine; it is REFUSED outright on an enforced node
//! (mirrors `hive_cloud::auth::enforced()`: `HIVE_JWT_SECRET` set and non-empty, checked directly
//! since this crate does not depend on `hive-cloud`), so an accidental ansible/env leak into
//! production cannot silently open every tenant-URL fetcher — only a genuinely unauthenticated dev
//! node honors it, and a boot WARN names the var the first time it is consulted.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::{Action, Attempt, Policy};
use reqwest::Url;

/// Redirects [`following_client`] and [`same_origin_client`] follow before giving up.
pub const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Mirrors `hive_cloud::auth::enforced()` without a cross-crate dependency: a node is enforced iff
/// `HIVE_JWT_SECRET` is set and non-empty. Kept in sync by hand; both read the same env var by the
/// same rule.
fn auth_enforced() -> bool {
    std::env::var("HIVE_JWT_SECRET").is_ok_and(|secret| !secret.is_empty())
}

fn dev_escape() -> bool {
    if !std::env::var("HIVE_OUTBOUND_ALLOW_PRIVATE").is_ok_and(|value| value == "1") {
        return false;
    }
    if auth_enforced() {
        static WARNED_REFUSED: OnceLock<()> = OnceLock::new();
        WARNED_REFUSED.get_or_init(|| {
            tracing::warn!(
                "HIVE_OUTBOUND_ALLOW_PRIVATE=1 is set but this node has auth enforced \
                 (HIVE_JWT_SECRET is set) — the escape is REFUSED; it only applies to an \
                 unauthenticated dev node"
            );
        });
        return false;
    }
    static WARNED_ACTIVE: OnceLock<()> = OnceLock::new();
    WARNED_ACTIVE.get_or_init(|| {
        tracing::warn!(
            "HIVE_OUTBOUND_ALLOW_PRIVATE=1 is set — every tenant-URL fetcher accepts http and \
             private/loopback addresses; never set this on a production node"
        );
    });
    true
}

/// Why a URL was refused: a fixed sentence, safe to hand back to the tenant (it never carries a
/// resolved address or an upstream status, so it is no oracle for internal hosts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "outbound URL refused: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// Whether `ip` is a public unicast address a tenant URL may reach.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4() {
        return public_v4(v4);
    }
    let s = ip.segments();
    let embedded =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if s[0] == 0x0064 && s[1] == 0xff9b {
        return s[2..6] == [0, 0, 0, 0] && public_v4(embedded(s[6], s[7]));
    }
    // SIIT's `::ffff:0:a.b.c.d` (the 0xffff marker one segment later than the standard v4-mapped
    // `::ffff:a.b.c.d` form `to_ipv4()` already handles above): `s[0..4]` zero, `s[4]==0xffff`,
    // `s[5]==0`, v4 in `s[6..8]`. Without this arm the embedded v4 address (which can be loopback
    // or RFC1918) is invisible to every check below and the whole address reads as public.
    if s[0..4] == [0, 0, 0, 0] && s[4] == 0xffff && s[5] == 0 {
        return public_v4(embedded(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return public_v4(embedded(s[1], s[2]));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        || (s[0] == 0x2001 && s[1] < 0x0200)
        || (s[0] == 0x3fff && s[1] < 0x1000)
        || (s[0] == 0x0100 && s[1..4] == [0, 0, 0]))
}

/// Names that mean "this machine or its network" whatever they resolve to, plus every single-label
/// name (a bare `metadata` or `redis` is resolved through the host's search domains).
fn internal_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    !host.contains('.')
        || [
            "localhost",
            "local",
            "internal",
            "localdomain",
            "home.arpa",
            "lan",
            "tencentyun.com",
        ]
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// The static check for a tenant-supplied URL the node sends a request or a secret to (webhooks,
/// callbacks, sinks): https (http only under the dev escape), no userinfo, a host that is not an
/// internal name or a non-public address literal.
pub fn validate_url(raw: &str) -> Result<Url, Refused> {
    check(raw, false)
}

/// [`validate_url`] for a plain fetch of a public resource (a remote image): http is allowed too.
pub fn validate_fetch_url(raw: &str) -> Result<Url, Refused> {
    check(raw, true)
}

fn check(raw: &str, allow_http: bool) -> Result<Url, Refused> {
    let url = Url::parse(raw.trim()).map_err(|_| Refused("not a valid absolute URL"))?;
    let dev = dev_escape();
    match url.scheme() {
        "https" => {}
        "http" if allow_http || dev => {}
        _ => return Err(Refused("only https URLs are allowed")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Refused("credentials in the URL are not allowed"));
    }
    let host = url.host_str().ok_or(Refused("the URL has no host"))?;
    if dev {
        return Ok(url);
    }
    match host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        Ok(ip) if !is_public(ip) => Err(Refused("the address is not a public one")),
        Ok(_) => Ok(url),
        Err(_) if internal_name(host) => Err(Refused("internal host names are not allowed")),
        Err(_) => Ok(url),
    }
}

/// Raised by the connect-time resolver when a name has no public address; [`describe`] recognises
/// it anywhere in an error chain.
#[derive(Debug)]
struct NoPublicAddress;

impl std::fmt::Display for NoPublicAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the host resolves to no public address")
    }
}

impl std::error::Error for NoPublicAddress {}

struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let found = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            let allowed: Vec<SocketAddr> = found
                .filter(|address| dev_escape() || is_public(address.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(Box::new(NoPublicAddress) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

fn public_hops(attempt: Attempt<'_>) -> Action {
    if attempt.previous().len() >= MAX_REDIRECTS {
        return attempt.error("too many redirects");
    }
    let plain_http_started = attempt
        .previous()
        .first()
        .is_some_and(|first| first.scheme() == "http");
    if check(attempt.url().as_str(), plain_http_started).is_ok() {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

fn same_origin_hops(attempt: Attempt<'_>) -> Action {
    let stays = attempt.previous().last().is_some_and(|from| {
        let to = attempt.url();
        from.scheme() == to.scheme()
            && from.host_str() == to.host_str()
            && from.port_or_known_default() == to.port_or_known_default()
    });
    if stays && attempt.previous().len() < MAX_REDIRECTS {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

fn public_only_builder(policy: Policy) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(PublicOnlyResolver))
        .redirect(policy)
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
}

/// The client for a tenant-supplied URL: public addresses only (checked at connect time), no
/// redirects, no proxy. Pair every request with [`validate_url`].
pub fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            public_only_builder(Policy::none())
                .build()
                .expect("the public-only HTTP client builds")
        })
        .clone()
}

/// [`client`] that follows up to [`MAX_REDIRECTS`] redirects, each hop passing the static check of
/// the request that started the chain ([`validate_url`], or [`validate_fetch_url`] when it began
/// on plain http); a hop that does not is returned to the caller as the 3xx it is.
pub fn following_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            public_only_builder(Policy::custom(public_hops))
                .build()
                .expect("the public-only following HTTP client builds")
        })
        .clone()
}

/// The client for a URL the platform builds toward a tenant's app: only a redirect that stays on
/// the same origin is followed, so the app cannot steer the node anywhere else.
pub fn same_origin_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(Policy::custom(same_origin_hops))
                .no_proxy()
                .build()
                .expect("the same-origin HTTP client builds")
        })
        .clone()
}

/// The client for the fleet's own calls (peers, providers): never follows a redirect.
pub fn internal_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(Policy::none())
                .build()
                .expect("the internal HTTP client builds")
        })
        .clone()
}

/// A fixed description of a failed request, safe to show a tenant: never the URL, the resolved
/// address or an OS error, so a delivery log is no oracle for the node's network.
pub fn describe(error: &reqwest::Error) -> &'static str {
    let mut link: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(cause) = link {
        if cause.is::<NoPublicAddress>() {
            return "the host does not resolve to a public address";
        }
        link = cause.source();
    }
    if error.is_timeout() {
        "timed out"
    } else if error.is_redirect() {
        "too many redirects"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "request failed"
    }
}

/// Reads a response body of at most `max` bytes. `None` when it is larger (never buffered past the
/// limit) or the transfer fails.
pub async fn read_limited(mut response: reqwest::Response, max: usize) -> Option<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > max {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}
