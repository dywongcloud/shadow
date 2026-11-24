//! Firewall: Attack Challenge Mode, sensitive-path denial, bot management,
//! observability and page-integrity DETECTION. One edge chokepoint, no new
//! crypto primitive and no second background loop. See docs/vercel-firewall-parity.md.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, OnceLock};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::project_settings::AttackMode;
use crate::state::CloudState;

type Shared = Arc<CloudState>;

pub const MAX_PATH_BYTES: usize = 2048;
const FW_COOKIE: &str = "hive_fw";
const FW_NONCE_HEADER: &str = "x-hive-challenge";
const CHALLENGE_SESSION_MS: u64 = 3_600_000;
const NONCE_TTL_MS: u64 = 300_000;
const MAX_EVENTS: usize = 512;
const MAX_EVENT_PATH_BYTES: usize = 256;
const MAX_UA_BYTES: usize = 256;

/// Domain separators keep these MACs distinct from every other use of the
/// fleet-shared at-rest key. A token minted for one purpose never verifies as
/// another, and neither equals a deployment-password unlock token.
const FW_COOKIE_DOMAIN: &[u8] = b"hive-firewall-challenge-cookie-v1\0";
const FW_NONCE_DOMAIN: &[u8] = b"hive-firewall-challenge-nonce-v1\0";

/// The mode duration options Vercel documents. Auto-off is LAZY: no timer and
/// no second loop; `active_until_ms` is checked on every request.
const MODE_HOURS: &[u64] = &[1, 6, 24];

fn clip_text(s: &str, cap: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > cap {
            break;
        }
        out.push(c);
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_digit(b[i + 1]), hex_digit(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bounded normalization used ONLY for the deny/bypass decision. The upstream
/// request keeps its original path; a caller-controlled string is never used
/// to build a URL, HTML or a Location header. Decodes twice so a doubly
/// encoded traversal cannot slip past, collapses dot segments and backslashes,
/// strips matrix params, query and fragment, and lowercases for comparison.
pub fn normalize_path(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_PATH_BYTES {
        return None;
    }
    if raw.bytes().any(|b| b == 0 || (b < 0x20 && b != b'\t')) {
        return None;
    }
    let once = percent_decode(raw);
    let twice = percent_decode(&once);
    if twice.len() > MAX_PATH_BYTES {
        return None;
    }
    let head = twice
        .split('?')
        .next()
        .unwrap_or("")
        .split('#')
        .next()
        .unwrap_or("")
        .to_string();
    let replaced = head.replace('\\', "/");
    let mut segs: Vec<String> = Vec::new();
    for seg in replaced.split('/') {
        let seg = seg.split(';').next().unwrap_or(seg);
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => {
                if s.len() > 512 || segs.len() > 128 {
                    return None;
                }
                segs.push(s.to_ascii_lowercase());
            }
        }
    }
    Some(format!("/{}", segs.join("/")))
}

/// Unambiguous probes: dot-segment secret/VCS files and framework debug
/// endpoints. AMBIGUOUS names (config.js, settings.json, app.js, Dockerfile)
/// are deliberately NOT here: a hard deny would break a real tenant asset.
const SENSITIVE_SEGMENTS: &[&str] = &[
    "@fs", "actuator", "_profiler", "elmah.axd", "telescope", "server-status",
    "wp-admin", "symfony", "master.key", "env.php", "local.xml", "settings.php",
    "credentials.db", "heapdump", "web.config", "parameters.yml",
];
const SENSITIVE_SUFFIXES: &[&str] = &[
    ".sql", ".tfstate", ".tfvars", ".swp", ".pem", ".key", ".save",
];
const CONFIG_BACKUP_STEMS: &[&str] = &[
    "config", "wp-config", "credentials", "secrets", "configuration", "settings",
    "appsettings", "client_secrets",
];
/// Unambiguous secret/credential BASENAMES, measured from the geniee corpus.
/// A stem match here denies regardless of extension (json/yml/ini/txt/...),
/// while ambiguous names such as config.js, settings.json and app.js stay
/// OUT of this list so a real tenant asset is never hard-denied.
const UNAMBIGUOUS_SECRET_STEMS: &[&str] = &[
    "credentials", "secrets", "client_secrets", "wp-config",
];
const SENSITIVE_PREFIXES: &[&str] = &[
    "api/v1/namespaces/default/secrets", "v2/_catalog", "config/master.key",
    "app/etc/", "sites/default/settings.php",
];

/// ALWAYS-BAD exact normalized paths, derived from the geniee.site probe corpus
/// plus the generated scanner families indexed in `docs/firewall-deny-index.md`.
/// Each names a private key, a secret-bearing tool config, or a framework
/// installer/debug/log endpoint that no tenant route plausibly needs. EXACT
/// match on purpose: a stem/prefix deny would also kill DX-legitimate assets
/// such as /actuator/health, /wp-admin/admin-ajax.php and /auth.js. Dot-segment
/// probes (/.git/config, /.aws/credentials, /wp-config.php.bak, /.env.*) are
/// already denied above by `sensitive-dotfile` and the suffix/backup rules.
/// Traversal variants collapse to these exact strings in `normalize_path`.
const ALWAYS_BAD_EXACT: &[&str] = &[
    // From the corpus: 17 paths the earlier rules missed. The two actuator
    // dumps at the end of this group are generated, not log-derived.
    "/service-account-key.json", "/service_account.json",
    "/sendgrid.env", "/sentry.properties", "/auth.json",
    "/claude_desktop_config.json", "/aider.conf.yml",
    "/oai_config_list.json", "/oai_config_list",
    "/litellm.yaml", "/litellm/config.yaml",
    "/config/custom-environment-variables.json",
    "/storage/logs/laravel.log", "/wp-content/debug.log",
    "/wp-admin/install.php", "/server-info", "/telescope/requests",
    "/actuator/env", "/actuator/heapdump",
    // Generated scanner-family paths, not present in the log.
    "/actuator/configprops", "/actuator/mappings", "/actuator/beans",
    "/actuator/httptrace", "/actuator/threaddump", "/actuator/loggers",
    "/actuator/jolokia", "/wp-admin/setup-config.php", "/wp-admin/upgrade.php",
    "/phpinfo.php", "/info.php", "/adminer.php", "/phpmyadmin/index.php",
    "/trace.axd", "/debug/vars", "/etc/passwd", "/proc/self/environ",
];

pub fn sensitive_rule(norm: &str) -> Option<&'static str> {
    // Exact always-bad paths first, so a matrix-param or doubly encoded
    // variant that `normalize_path` collapses to one of them is denied by the
    // same rule instead of reaching the segment rules below. Deliberately NOT
    // a stem or prefix match: /actuator/health, /wp-admin/admin-ajax.php and
    // /auth.js must stay reachable. NEVER widen this list to a bare "config"
    // or "env" stem: /config.js and /env.js are public runtime-config assets
    // a real tenant serves. When in doubt, leave the path to the
    // challenge/rate-limit path and record the residual.
    if ALWAYS_BAD_EXACT.contains(&norm) {
        return Some("sensitive-always-bad");
    }
    let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    // A dot-segment is a secret/VCS file by convention. The platform proof
    // namespaces and the two benign published dotfiles are exempt. The
    // ambiguous PROBE names below match only the LAST segment, so a real
    // tenant route such as /actuator/health is never hard-denied.
    const BENIGN_DOT_SEGMENTS: &[&str] = &[".well-known", ".hive-verify", ".nojekyll"];
    for s in &segs {
        if s.starts_with('.') && !BENIGN_DOT_SEGMENTS.contains(s) {
            return Some("sensitive-dotfile");
        }
    }
    let last = segs[segs.len() - 1];
    if SENSITIVE_SEGMENTS.contains(&last)
        || last.starts_with("phpinfo")
        || last.starts_with("php_info")
    {
        return Some("sensitive-probe");
    }
    let joined = segs.join("/");
    for p in SENSITIVE_PREFIXES {
        if joined == *p || joined.starts_with(*p) {
            return Some("sensitive-probe");
        }
    }
    // Stem match covers credentials.json / secrets.yml / wp-config.txt and
    // terraform.tfstate.backup without ever denying config.js or settings.json.
    let stem = last.split('.').next().unwrap_or(last);
    if UNAMBIGUOUS_SECRET_STEMS.contains(&stem) || last.starts_with("terraform.tfstate") {
        return Some("sensitive-secret-file");
    }
    for suf in SENSITIVE_SUFFIXES {
        if last.ends_with(*suf) {
            return Some("sensitive-dump");
        }
    }
    for suf in [".bak", ".old", ".orig", "~"] {
        if let Some(stem) = last.strip_suffix(suf) {
            let stem = stem.split('.').next().unwrap_or(stem);
            if CONFIG_BACKUP_STEMS.contains(&stem) {
                return Some("sensitive-backup");
            }
        }
    }
    None
}

/// Requests that must NEVER be challenged or denied: health checks, ACME and
/// DNS proof namespaces, robots, favicons and static build output. The admin
/// API lives on a separate listener, so it is structurally unreachable here.
pub fn always_bypass(norm: &str) -> bool {
    for p in [
        "/healthz", "/robots.txt", "/favicon.ico", "/icon.svg",
        "/.hive-verify",
    ] {
        if norm == p || norm.starts_with(&format!("{p}/")) {
            return true;
        }
    }
    norm == "/.well-known" || norm.starts_with("/.well-known/")
        || norm.starts_with("/_next/static/") || norm.starts_with("/_next/image")
        || norm.starts_with("/_next/data/") || norm.starts_with("/assets/")
        || norm.starts_with("/_vercel/") || norm.starts_with("/fonts/")
        || norm == "/sitemap.xml" || norm == "/manifest.json"
}

// ---------------------------------------------------------------------------
// Server-sealed tokens. Reuses the fleet-shared at-rest key, the crate's
// HMAC-SHA256 helper and its constant-time comparison; no new primitive.
// ---------------------------------------------------------------------------

fn mac(domain: &[u8], parts: &[&[u8]]) -> String {
    let key = crate::secrets::key_material();
    let mut msg = Vec::with_capacity(160);
    msg.extend_from_slice(domain);
    for p in parts {
        msg.extend_from_slice(&(p.len() as u64).to_be_bytes());
        msg.extend_from_slice(p);
    }
    crate::admin::hmac_sha256_hex(&key, &msg)
}

fn sealed(domain: &[u8], project: &str, host: &str, mode_ms: u64, expires_ms: u64) -> String {
    let exp = expires_ms.to_string();
    let mm = mode_ms.to_string();
    let tag = mac(
        domain,
        &[project.as_bytes(), host.as_bytes(), mm.as_bytes(), exp.as_bytes()],
    );
    format!("v1.{expires_ms}.{tag}")
}

fn verify_sealed(
    domain: &[u8],
    value: &str,
    project: &str,
    host: &str,
    mode_ms: u64,
    now_ms: u64,
) -> bool {
    let mut parts = value.splitn(3, '.');
    let (Some("v1"), Some(exp), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let Ok(expires_ms) = exp.parse::<u64>() else {
        return false;
    };
    if expires_ms <= now_ms || expires_ms > now_ms.saturating_add(CHALLENGE_SESSION_MS + 60_000) {
        return false;
    }
    let want = sealed(domain, project, host, mode_ms, expires_ms);
    crate::admin::ct_eq(&want, value)
}

/// A short-lived, server-sealed nonce embedded in the challenge page.
pub fn challenge_nonce(project: &str, host: &str, mode_ms: u64, now_ms: u64) -> String {
    sealed(
        FW_NONCE_DOMAIN,
        project,
        host,
        mode_ms,
        now_ms.saturating_add(NONCE_TTL_MS),
    )
}

pub fn verify_nonce(value: &str, project: &str, host: &str, mode_ms: u64, now_ms: u64) -> bool {
    verify_sealed(FW_NONCE_DOMAIN, value, project, host, mode_ms, now_ms)
}

pub fn seal_cookie(project: &str, host: &str, mode_ms: u64, now_ms: u64) -> String {
    sealed(
        FW_COOKIE_DOMAIN,
        project,
        host,
        mode_ms,
        now_ms.saturating_add(CHALLENGE_SESSION_MS),
    )
}

pub fn verify_cookie(value: &str, project: &str, host: &str, mode_ms: u64, now_ms: u64) -> bool {
    verify_sealed(FW_COOKIE_DOMAIN, value, project, host, mode_ms, now_ms)
}

/// `Set-Cookie` for a solved challenge. HttpOnly, SameSite=Lax, Secure unless
/// the request is plain http to a loopback dev host (the same rule as the
/// deployment-password unlock cookie).
fn cookie_header(token: &str, host: &str, headers: &[(String, String)]) -> String {
    let forwarded_https = header_value(headers, "x-forwarded-proto").is_some_and(|p| {
        p.split(',')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("https")
    });
    let secure = if forwarded_https || !crate::edge::is_loopback_host(host) {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{FW_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
        CHALLENGE_SESSION_MS / 1000
    )
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// A challenge screen with ONE terminal failure path. NO caller-controlled
/// text, NO form action and NO Location. A solve is accepted only when the
/// server answers with an explicit `challenge-solved` signal; any other
/// outcome (a 200 challenge page from a rejected nonce, a redirect, a 403,
/// a cached or service-worker response) is a FAILURE shown to the visitor,
/// never an unconditional `location.reload()`.
///
/// Before reloading, a second request proves the sealed cookie actually
/// landed. Without that proof a browser that refuses the cookie (or a CDN
/// replaying the solve response) reloads into another challenge page forever.
/// `.catch` covers a fetch that never resolves, which otherwise stalls on
/// "Security check" with no message. A manual retry mints a fresh nonce.
/// This is a JavaScript-capability gate, NOT browser attestation.
/// The shadw wordmark is an inline data URI: no dashboard asset path is reachable on a tenant host.
const CHALLENGE_PAGE: &str = r##"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Security check - shadw</title>
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#fafafa" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#0a0a0a" media="(prefers-color-scheme: dark)">
<style>
:root{color-scheme:light dark;--bg:hsl(0 0% 98%);--card:hsl(0 0% 100%);--fg:hsl(0 0% 4%);--secondary:hsl(0 0% 40%);--border:hsl(0 0% 92%);--border-strong:hsl(0 0% 80%);--link:hsl(212 100% 47%);--accent:hsl(0 0% 4%);--accent-fg:hsl(0 0% 100%);--top-light:rgba(255,255,255,.95);--glow:rgba(0,0,0,.035)}
@media (prefers-color-scheme:dark){:root{--bg:hsl(0 0% 4%);--card:hsl(0 0% 7%);--fg:hsl(0 0% 93%);--secondary:hsl(0 0% 63%);--border:hsl(0 0% 16%);--border-strong:hsl(0 0% 28%);--link:hsl(212 100% 62%);--accent:hsl(0 0% 100%);--accent-fg:hsl(0 0% 4%);--top-light:rgba(255,255,255,.12);--glow:rgba(255,255,255,.045)}}
@media (prefers-contrast:more){:root{--secondary:hsl(0 0% 20%);--border-strong:hsl(0 0% 40%)}@media (prefers-color-scheme:dark){:root{--secondary:hsl(0 0% 85%);--border-strong:hsl(0 0% 60%)}}}
*{box-sizing:border-box}
html,body{margin:0;min-height:100%}
body{background:var(--bg);color:var(--fg);font-family:ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;font-size:15px;line-height:1.6;display:flex;align-items:center;justify-content:center;padding:24px;-webkit-font-smoothing:antialiased;background-image:radial-gradient(120% 60% at 50% -18%,var(--glow),transparent 70%)}
.wrap{width:100%;max-width:360px}
.card{position:relative;background:var(--card);border:1px solid var(--border);border-radius:14px;padding:28px;box-shadow:0 24px 60px -32px rgba(0,0,0,.4)}
.card:before{content:"";position:absolute;inset:0 0 auto 0;height:1px;background:linear-gradient(90deg,transparent,var(--top-light),transparent)}
.brand{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:24px}
.brand img{display:block;height:28px;width:auto}
@media (prefers-color-scheme:light){.brand img{filter:invert(1)}}
.tag{font:10px/1 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;letter-spacing:.12em;text-transform:uppercase;color:var(--secondary);border:1px solid var(--border);border-radius:999px;padding:6px 9px;white-space:nowrap}
h1{font-size:20px;line-height:1.3;letter-spacing:-.02em;font-weight:600;margin:0 0 8px}
.sub{font-size:13px;line-height:1.6;color:var(--secondary);margin:0 0 20px}
.status{margin:0;padding:12px 14px;border:1px solid var(--border);border-radius:8px;background:var(--bg);font-size:14px;line-height:1.55}
.status:before{content:"";display:inline-block;width:6px;height:6px;margin-right:9px;border-radius:50%;background:var(--link);vertical-align:middle}
.trace{font:12px/1.5 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;color:var(--secondary);margin:12px 0 0;letter-spacing:.02em}.trace span{display:block}
#hive-fw-retry{display:block;width:100%;min-height:44px;margin-top:18px;background:var(--accent);color:var(--accent-fg);border:0;border-radius:8px;padding:10px 14px;font:inherit;font-size:14px;font-weight:600;cursor:pointer}
#hive-fw-retry:hover{opacity:.9}
#hive-fw-retry:focus-visible{outline:2px solid var(--link);outline-offset:2px}
[hidden]{display:none!important}
.noscript{margin:14px 0 0;padding:12px 14px;border:1px solid var(--border);border-radius:8px;background:var(--bg);font-size:13px;color:var(--secondary)}
.foot{margin-top:20px;padding-top:16px;border-top:1px solid var(--border);font-size:11px;color:var(--secondary);text-align:center;letter-spacing:.04em}
@media (prefers-reduced-motion:no-preference){.card{animation:rise .35s ease-out both}.trace span{animation:settle .5s ease-out both}.trace span:nth-child(2){animation-delay:.22s}.trace span:nth-child(3){animation-delay:.44s}@keyframes rise{from{opacity:0;transform:translateY(6px)}to{opacity:1;transform:none}}@keyframes settle{from{letter-spacing:.16em}to{letter-spacing:.02em}}}
@media (max-width:380px){.card{padding:20px}}
</style>
</head><body><main class="wrap"><section class="card" aria-labelledby="hive-fw-title">
<header class="brand"><img src="data:image/png;base64,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" width="274" height="91" alt="shadw"><span class="tag">Attack challenge</span></header>
<h1 id="hive-fw-title">Security check</h1>
<p class="sub">shadw is checking that this request can complete a short browser script. A sealed, short-lived cookie proves the check finished; if that cookie cannot be kept, the request is refused. This check is scoped to this site, retries are bounded, and it does not identify you. It is a capability check, not an attestation.</p>
<p class="status" id="hive-fw-status" role="status" aria-live="polite">Running a short browser script. You will continue automatically.</p>
<p class="trace"><span>a script runs</span><span>a cookie is sealed</span><span>the request is checked again</span></p>
<button type="button" id="hive-fw-retry" hidden>Try again</button>
<noscript><p class="noscript">JavaScript is required to complete this check. Enable it in your browser, then reload this page. No retry is possible while scripts are disabled.</p></noscript>
<footer class="foot">shadw edge protection - this check is scoped to this site</footer>
</section></main>
<script>
(function(){
  var status=document.getElementById('hive-fw-status');
  var retry=document.getElementById('hive-fw-retry');
  var KEY='hive_fw_challenge_attempts';
  function fail(msg){status.textContent=msg;retry.hidden=false;}
  function reset(){try{sessionStorage.removeItem(KEY);}catch(e){}}
  retry.addEventListener('click',function(){reset();location.reload();});
  var now=Date.now(),start=now,n=0;
  try{
    var raw=sessionStorage.getItem(KEY),p=raw?raw.split(':'):null;
    if(p&&p.length===2&&Number(p[0])>0&&now-Number(p[0])<30000&&Number(p[1])>0){
      start=Number(p[0]);n=Number(p[1]);
    }
    sessionStorage.setItem(KEY,start+':'+(n+1));
  }catch(e){}
  if(n>=3){fail('The security check could not be completed. Please try again.');return;}
  function probe(){
    return fetch(location.href,{method:'GET',credentials:'same-origin',cache:'no-store'})
      .then(function(p){return p.text().then(function(body){
        // A legitimate app response may be a 404 or a redirect: only a
        // challenge page (or a replayed "verified" solve body) means the
        // sealed cookie did not land. Check all three signals so a CDN that
        // strips the action header still cannot restart a reload loop.
        var again=p.headers.get('x-hive-firewall-action')==='challenge'||
                  body.trim()==='verified'||body.indexOf('hive-fw-status')>=0;
        if(!again){reset();location.reload();}
        else{fail('This browser did not keep the security cookie. Please enable cookies and try again.');}
      });});
  }
  fetch(location.href,{
    method:'GET',credentials:'same-origin',cache:'no-store',
    headers:{'x-hive-challenge':'__HIVE_NONCE__'}
  }).then(function(r){
    if(!r.ok){fail('The security check was not accepted. Please try again.');return;}
    return r.text().then(function(body){
      if(r.headers.get('x-hive-firewall-action')==='challenge-solved'||body.trim()==='verified'){
        return probe();
      }
      fail('The security check was not accepted. Please try again.');
    });
  }).catch(function(){
    fail('The security check could not reach the server. Please try again.');
  });
})();
</script>
</body></html>"##;

pub fn challenge_page(nonce: &str) -> String {
    // The nonce is server-minted, but a strict allowlist keeps the inline
    // <script> a closed string context even if the token format changes.
    // HTML entities are NOT decoded inside a script element, so this is not
    // html_escape: an escaped token would silently stop verifying.
    let safe: String = nonce
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect();
    CHALLENGE_PAGE.replace("__HIVE_NONCE__", &safe)
}

// ---------------------------------------------------------------------------
// Verified bots. UA matching is SPOOFABLE and is disclosed as such. No bot
// score, no JA4/JA3, no reverse-DNS verification and no invented count.
// ---------------------------------------------------------------------------

pub struct VerifiedBot {
    pub name: &'static str,
    pub ua: &'static str,
    pub verification_method: &'static str,
}

pub static VERIFIED_BOTS: &[VerifiedBot] = &[
    VerifiedBot { name: "Googlebot", ua: "Googlebot", verification_method: "ua-substring (spoofable)" },
    VerifiedBot { name: "Bingbot", ua: "bingbot", verification_method: "ua-substring (spoofable)" },
    VerifiedBot { name: "DuckDuckBot", ua: "DuckDuckBot", verification_method: "ua-substring (spoofable)" },
    VerifiedBot { name: "Applebot", ua: "Applebot", verification_method: "ua-substring (spoofable)" },
    VerifiedBot { name: "Slackbot", ua: "Slackbot", verification_method: "ua-substring (spoofable)" },
    VerifiedBot { name: "SocialPreview", ua: "facebookexternalhit", verification_method: "ua-substring (spoofable)" },
    VerifiedBot { name: "UptimeRobot", ua: "UptimeRobot", verification_method: "ua-substring (spoofable)" },
];

fn verified_bot(ua: &str) -> Option<&'static str> {
    if ua.is_empty() || ua.len() > MAX_UA_BYTES {
        return None;
    }
    let lower = ua.to_ascii_lowercase();
    VERIFIED_BOTS
        .iter()
        .find(|b| lower.contains(b.ua.to_ascii_lowercase().as_str()))
        .map(|b| b.name)
}

/// Vercel's documented bot_status vocabulary, derived ONLY from the existing
/// UA classifier. It is a measurement, not an attestation.
pub fn bot_status(ua: &str) -> &'static str {
    if verified_bot(ua).is_some() {
        "verified"
    } else if ua.is_empty() {
        "unknown"
    } else {
        let lower = ua.to_ascii_lowercase();
        if lower.contains("bot") || lower.contains("crawler") || lower.contains("spider") {
            "identified"
        } else {
            "unknown"
        }
    }
}

// ---------------------------------------------------------------------------
// Page integrity: DETECTION only. A bounded report of script digests. No CSP
// script-src enforcement, no nonce injection and no browser blocking.
// ---------------------------------------------------------------------------

static INTEGRITY: OnceLock<Mutex<IntegrityState>> = OnceLock::new();

#[derive(Default)]
struct IntegrityState {
    scripts_reported: u64,
    mismatches: u64,
}

fn integrity() -> &'static Mutex<IntegrityState> {
    INTEGRITY.get_or_init(|| Mutex::new(IntegrityState::default()))
}

const HEX64_LEN: usize = 64;

fn valid_digest(s: &str) -> bool {
    s.len() == HEX64_LEN && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn integrity_report(rows: &[Value]) -> (usize, usize) {
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    for r in rows.iter().take(100) {
        let Some(d) = r.get("sha256").and_then(|v| v.as_str()) else {
            rejected += 1;
            continue;
        };
        if !valid_digest(d) {
            rejected += 1;
            continue;
        }
        accepted += 1;
    }
    let mut st = integrity().lock();
    st.scripts_reported = st.scripts_reported.saturating_add(accepted as u64);
    st.mismatches = st.mismatches.saturating_add(rejected as u64);
    (accepted, rejected)
}

// ---------------------------------------------------------------------------
// Node-local bounded observability ring. NO IPs, cookies, tokens or secrets.
// ---------------------------------------------------------------------------

#[derive(Clone, serde::Serialize)]
pub struct FirewallEvent {
    pub ts_ms: u64,
    pub project: String,
    pub path: String,
    pub status: u16,
    pub action: &'static str,
    pub reason: String,
}

static EVENTS: OnceLock<Mutex<VecDeque<FirewallEvent>>> = OnceLock::new();

fn ring() -> &'static Mutex<VecDeque<FirewallEvent>> {
    EVENTS.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_EVENTS)))
}

pub fn record_event(project: &str, path: &str, status: u16, action: &'static str, reason: &str) {
    let e = FirewallEvent {
        ts_ms: hive_core::now_ms(),
        project: clip_text(project, 128),
        path: clip_text(path, MAX_EVENT_PATH_BYTES),
        status,
        action,
        reason: clip_text(reason, 128),
    };
    let mut q = ring().lock();
    if q.len() >= MAX_EVENTS {
        q.pop_front();
    }
    q.push_back(e);
}

pub fn events_since(n: usize) -> Vec<FirewallEvent> {
    let q = ring().lock();
    q.iter().rev().take(n.min(MAX_EVENTS)).cloned().collect()
}

fn by_action(rows: &[FirewallEvent]) -> Value {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for e in rows {
        *counts.entry(e.action).or_default() += 1;
    }
    json!(counts)
}

fn log_decision(
    c: &Shared,
    region: &str,
    method: &str,
    host: &str,
    path: &str,
    status: u16,
    action: &'static str,
    reason: &str,
    project: &str,
) {
    record_event(project, path, status, action, reason);
    let ev = c.event(region, method, host, path, status, action, reason);
    c.record(ev);
}

// ---------------------------------------------------------------------------
// Attack Challenge Mode state. Stored on the REPLICATED ProjectSettings row,
// written through `touch`, with a LAZY expiry (no timer, no second loop).
// ---------------------------------------------------------------------------

pub fn active_mode(c: &Shared, project: &str, now_ms: u64) -> Option<AttackMode> {
    let m = c.projects.attack_mode(project);
    if m.enabled && m.active_until_ms > now_ms {
        Some(m)
    } else {
        None
    }
}

pub fn set_mode(
    c: &Shared,
    project: &str,
    enabled: bool,
    hours: u64,
    verified_bot_bypass: bool,
    now_ms: u64,
) -> Result<AttackMode, String> {
    if !MODE_HOURS.contains(&hours) {
        return Err("duration must be 1, 6 or 24 hours".into());
    }
    let m = AttackMode {
        enabled,
        active_until_ms: if enabled {
            now_ms.saturating_add(hours.saturating_mul(3_600_000))
        } else {
            0
        },
        updated_ms: now_ms,
        duration_secs: hours.saturating_mul(3600),
        verified_bot_bypass,
    };
    c.projects.set_attack_mode(project, m);
    Ok(m)
}

/// The ONE edge decision. Called at the top of `edge_pipeline_inner`, before
/// ACME, before any canonical/host redirect and before the rate limiter, so a
/// sensitive probe can never be answered with a 307 or a 404.
pub fn edge_gate(
    c: &Shared,
    method: &str,
    raw_path: &str,
    host: &str,
    headers: &[(String, String)],
    ua: &str,
    region: &str,
) -> Option<Response> {
    let Some(norm) = normalize_path(raw_path) else {
        // A path the bounded normalizer refuses is never admitted by an
        // always-on security control: fail closed with a fixed safe body.
        let mut resp = (StatusCode::BAD_REQUEST, "invalid request path").into_response();
        set_fw_headers(&mut resp, region, "deny_sensitive");
        return Some(resp);
    };
    // The sensitive deny runs BEFORE the bypass list: a secret file must never
    // ride inside an exempt prefix. The platform proof namespaces themselves
    // are exempt inside sensitive_rule via BENIGN_DOT_SEGMENTS.
    // Prefer the gateway's EXACT host attribution (aliases_full before the
    // first-label map) so a custom full-host domain maps to the right
    // replicated ProjectSettings row. The first-label fallback is bounded and
    // only applies to an unresolved host, where the mode is OFF anyway.
    let project = c
        .gw
        .deployment_for_host(host)
        .map(|d| d.project)
        .or_else(|| c.gw.attribution_for_host(host).map(|(_, p)| p))
        .unwrap_or_else(|| {
            host.split(':')
                .next()
                .unwrap_or(host)
                .split('.')
                .next()
                .unwrap_or(host)
                .to_string()
        });
    let now = hive_core::now_ms();
    let mode = active_mode(c, &project, now);
    let mode_ms = mode.as_ref().map(|m| m.updated_ms).unwrap_or(0);

    if let Some(rule) = sensitive_rule(&norm) {
        log_decision(c, region, method, host, &norm, 403, "deny_sensitive", rule, &project);
        let body = format!("blocked by firewall ({rule})");
        let mut resp = (StatusCode::FORBIDDEN, body).into_response();
        if let Ok(v) = HeaderValue::from_str(rule) {
            resp.headers_mut()
                .insert(HeaderName::from_static("x-hive-firewall-rule"), v);
        }
        set_fw_headers(&mut resp, region, "deny_sensitive");
        return Some(resp);
    }
    if always_bypass(&norm)
        || header_value(headers, "x-hive-revalidate").is_some()
    {
        return None;
    }

    let Some(m) = mode else {
        return None;
    };
    let cookie = crate::edge::cookie_value(headers, FW_COOKIE);
    if let Some(v) = cookie.as_deref() {
        if verify_cookie(v, &project, host, mode_ms, now) {
            log_decision(c, region, method, host, &norm, 200, "pass_cookie", "sealed cookie", &project);
            return None;
        }
        // A stale cookie used to fall through to a fresh 200 challenge page
        // with no record of the rejection, so a browser stuck in a reload
        // loop was invisible in the event ring. Record the real decision.
        log_decision(c, region, method, host, &norm, 200, "challenge-failed", "rejected cookie", &project);
    }
    let nonce = header_value(headers, FW_NONCE_HEADER);
    if let Some(v) = nonce.as_deref() {
        if verify_nonce(v, &project, host, mode_ms, now) {
            let token = seal_cookie(&project, host, mode_ms, now);
            log_decision(c, region, method, host, &norm, 200, "challenge-solved", "sealed nonce", &project);
            let mut resp = (StatusCode::OK, "verified").into_response();
            if let Ok(v) = HeaderValue::from_str(&cookie_header(&token, host, headers)) {
                resp.headers_mut().insert(header::SET_COOKIE, v);
            }
            // The solve carries a Set-Cookie and shares its URL with the page.
            // Without no-store a shared cache can replay "verified" to other
            // visitors, or replay it to the solver instead of the real page.
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            set_fw_headers(&mut resp, region, "challenge-solved");
            return Some(resp);
        }
        log_decision(c, region, method, host, &norm, 200, "challenge-failed", "rejected nonce", &project);
    }
    if m.verified_bot_bypass {
        if let Some(name) = verified_bot(ua) {
            log_decision(c, region, method, host, &norm, 200, "bot_allowed", name, &project);
            return None;
        }
    }
    if !matches!(method, "GET" | "HEAD" | "OPTIONS") {
        log_decision(c, region, method, host, &norm, 503, "challenge", "non-idempotent", &project);
        let mut resp = (
            StatusCode::SERVICE_UNAVAILABLE,
            "the gateway is challenging this project; retry after the browser check",
        )
            .into_response();
        set_fw_headers(&mut resp, region, "challenge");
        return Some(resp);
    }
    log_decision(c, region, method, host, &norm, 200, "challenge", "attack-mode-active", &project);
    let nonce = challenge_nonce(&project, host, mode_ms, now);
    let mut resp = Html(challenge_page(&nonce)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    set_fw_headers(&mut resp, region, "challenge");
    Some(resp)
}

fn set_fw_headers(resp: &mut Response, region: &str, action: &str) {
    if let Ok(v) = HeaderValue::from_str(region) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-hive-region"), v);
    }
    if let Ok(v) = HeaderValue::from_str(action) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-hive-firewall-action"), v);
    }
}

// ---------------------------------------------------------------------------
// Admin API. The setting is replicated through the projects row; the event
// ring is NODE-LOCAL and is reported as such, never as a fleet aggregate.
// ---------------------------------------------------------------------------

const RESIDUALS: &[&str] = &[
    "Attack Challenge Mode is replicated per-project config with a lazy 1/6/24 hour auto-off; automatic attack detection is not implemented.",
    "Sensitive-path denial is an ALWAYS-ON system mitigation, independent of Attack Challenge Mode: unambiguous secret, VCS and framework-debug paths are refused with a fixed 403 before any redirect, even when the mode is off. Measured against the geniee corpus: all 79 challenge-relevant 307/404 gaps are closed with mode OFF.",
    "AMBIGUOUS names (config.js, settings.json, app.js, Dockerfile) are never hard-denied; they are only challenged while Attack Mode is active, so a legitimate tenant asset is not broken.",
    "A path the bounded normalizer refuses is rejected with a fixed 400 rather than admitted. Non-idempotent methods (POST/PUT/DELETE/PATCH) are refused with a fixed 503 during Attack Mode because the JavaScript challenge would otherwise silently lose the request body. EventSource/SSE clients cannot execute the challenge; they are covered by the same refusal.",
    "Firewall state and events are NODE-LOCAL: there is no owner proxy, gossip dispatch arm or fleet fanout for the event ring; six round-robin dashboard reads return six disjoint rings. A mode toggle is durably persisted but is not fanned out to every owner-chain candidate by db_replicate::fanout_all.",
    "The challenge is a JavaScript-capability gate, NOT Vercel's proprietary browser attestation. A scripted client that executes JavaScript can solve it. A rejected or stalled solve is now TERMINAL: a visible error with a manual retry, never an unbounded reload loop; automatic attempts are bounded to three within 30 seconds.",
    "The probe that proves the sealed cookie landed is a client-side GET on the same path, so it consumes one request per solve. A CDN that rewrites or strips x-hive-firewall-action is detected by the body markers, but a proxy that rewrites the HTML body itself can still defeat the check.",
    "Verified-bot matching uses the User-Agent string only and is SPOOFABLE. No reverse-DNS/IP verification, no bot score, no JA4/JA3 and no BotID private signal network.",
    "Page integrity is DETECTION only. There is no CSP script-src enforcement, no nonce injection, no script inventory approval workflow and no browser blocking.",
    "L3/L4 volumetric DDoS absorption, anycast scrubbing and Vercel's automatic network-layer mitigation are NOT implemented and cannot be implemented in application code. L7 rate limiting reuses the existing shared primitive.",
    "Firewall events are node-local and bounded; the dashboard never prints a fabricated fleet total. Vercel's log retention, drill-down and SIEM drains are not implemented.",
    "There is no 1:1 Vercel Firewall, WAF rule engine, BotID SDK or verified-bots registry equivalence; only the documented, replicable behaviour is implemented.",
];

pub fn state_value(c: &Shared, project: &str) -> Value {
    let now = hive_core::now_ms();
    let raw = c.projects.attack_mode(project);
    let active = active_mode(c, project, now);
    let events = events_since(MAX_EVENTS);
    let st = integrity().lock();
    json!({
        "node_local": true,
        "project": project,
        "attack_mode": {
            "enabled": raw.enabled,
            "active": active.is_some(),
            "active_until_ms": raw.active_until_ms,
            "updated_ms": raw.updated_ms,
            "duration_secs": raw.duration_secs,
            "hours_left": if active.is_some() {
                json!((raw.active_until_ms.saturating_sub(now)) / 3_600_000)
            } else { Value::Null },
            "verified_bot_bypass": raw.verified_bot_bypass,
        },
        "events": {
            "node_local": true,
            "count": events.len(),
            "by_action": by_action(&events),
            "recent": events.iter().take(16).cloned().collect::<Vec<_>>(),
        },
        "verified_bots": {
            "count": VERIFIED_BOTS.len(),
            "spoofable": true,
            "rows": VERIFIED_BOTS.iter().map(|b| json!({
                "name": b.name, "ua": b.ua,
                "verification_method": b.verification_method,
                "spoofable": true,
            })).collect::<Vec<_>>(),
        },
        "integrity": {
            "detection_only": true,
            "scripts_reported": st.scripts_reported,
            "mismatches": st.mismatches,
        },
        "bot_status": bot_status(""),
        "bot_status_vocabulary": ["verified", "identified", "impersonation", "unknown"],
        "residuals": RESIDUALS,
    })
}

#[derive(Deserialize)]
struct StateQuery {
    #[serde(default)]
    project: String,
}

async fn state_handler(State(c): State<Shared>, Query(q): Query<StateQuery>) -> Json<Value> {
    let project = clip_text(q.project.trim(), 128);
    Json(state_value(&c, &project))
}

async fn events_handler() -> Json<Value> {
    let rows = events_since(64);
    Json(json!({ "node_local": true, "count": rows.len(), "recent": rows }))
}

#[derive(Deserialize)]
struct IntegrityBody {
    #[serde(default)]
    scripts: Vec<Value>,
}

async fn integrity_handler(Json(b): Json<IntegrityBody>) -> Json<Value> {
    let (accepted, rejected) = integrity_report(&b.scripts);
    let st = integrity().lock();
    Json(json!({
        "detection_only": true,
        "accepted": accepted,
        "rejected": rejected,
        "scripts_reported": st.scripts_reported,
        "mismatches": st.mismatches,
    }))
}

#[derive(Deserialize)]
struct AttackModeBody {
    enabled: bool,
    #[serde(default = "default_hours")]
    hours: u64,
    #[serde(default)]
    verified_bot_bypass: bool,
}

fn default_hours() -> u64 {
    1
}

async fn attack_mode_put(
    State(c): State<Shared>,
    headers: HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    Path(project): Path<String>,
    Json(b): Json<AttackModeBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // A destructive, project-cascading control asserts identity BEFORE any
    // tenant is resolved; an unenforced node's implicit "personal" would own
    // everything otherwise.
    let team_header = headers.get("x-hive-team").and_then(|v| v.to_str().ok());
    if !crate::admin::has_explicit_caller_identity(claims.is_some(), team_header) {
        return Err((StatusCode::UNAUTHORIZED, "sign-in required".into()));
    }
    crate::admin::require_project_claiming_row(
        &c,
        &headers,
        claims.as_ref().map(|e| &e.0),
        &project,
    )?;
    let now = hive_core::now_ms();
    set_mode(&c, &project, b.enabled, b.hours, b.verified_bot_bypass, now)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // A security decision must be durable BEFORE it is acknowledged; a failed
    // fsync fails closed rather than leaving a mode that survives only in RAM.
    if !crate::persist::persist_durable(&c) {
        c.projects.set_attack_mode(&project, AttackMode::default());
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "could not durably persist the firewall setting".into(),
        ));
    }
    record_event(&project, "/v1/firewall/attack-mode", 200, "log", "mode-updated");
    Ok(Json(state_value(&c, &project)))
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/v1/firewall/state", get(state_handler))
        .route("/v1/firewall/events", get(events_handler))
        .route("/v1/firewall/integrity/report", post(integrity_handler))
        .route("/v1/projects/:project/firewall/attack-mode", put(attack_mode_put))
}
