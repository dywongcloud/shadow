//! Web Application Firewall — a custom rule engine plus a managed ruleset for
//! common attack signatures (SQLi / XSS / path traversal). Modeled on Vercel's
//! WAF: ordered rules with allow/deny/log actions evaluated at the edge.

use parking_lot::RwLock;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WafAction {
    /// Allow immediately (short-circuits later rules).
    Allow,
    /// Deny with 403.
    Deny,
    /// Allow but record (visible in analytics).
    Log,
}

/// What a rule matches on. All present fields must match (AND).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WafMatch {
    /// Regex against the request path.
    pub path_regex: Option<String>,
    /// Exact method (GET/POST/...).
    pub method: Option<String>,
    /// Client IP prefix (e.g. "10.0." matches 10.0.x.x).
    pub ip_prefix: Option<String>,
    /// Header `name` must contain `contains` (case-insensitive).
    pub header_contains: Option<(String, String)>,
    /// Substring that must appear in path+query (case-insensitive).
    pub contains: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WafRule {
    pub id: String,
    pub description: String,
    pub action: WafAction,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub when: WafMatch,
}

fn default_true() -> bool {
    true
}

/// Read-only view of a request for evaluation.
pub struct RequestCtx<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub ip: &'a str,
    pub headers: &'a [(String, String)],
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny { rule_id: String, reason: String },
}

struct Compiled {
    rule: WafRule,
    path_re: Option<Regex>,
}

pub struct Waf {
    inner: RwLock<Vec<Compiled>>,
    managed: RwLock<bool>,
    // Managed signature regexes.
    sqli: Regex,
    xss: Regex,
    traversal: Regex,
}

impl Waf {
    pub fn new() -> Arc<Waf> {
        Arc::new(Waf {
            inner: RwLock::new(Vec::new()),
            managed: RwLock::new(true),
            sqli: Regex::new(r"(?i)(union\s+select|or\s+1\s*=\s*1|;\s*drop\s+table|/\*|--\s|xp_cmdshell)")
                .unwrap(),
            xss: Regex::new(r"(?i)(<script|onerror\s*=|onload\s*=|javascript:|<img[^>]+src)")
                .unwrap(),
            traversal: Regex::new(r"(\.\./|\.\.%2f|%2e%2e/)").unwrap(),
        })
    }

    pub fn set_managed(&self, on: bool) {
        *self.managed.write() = on;
    }

    pub fn managed_enabled(&self) -> bool {
        *self.managed.read()
    }

    pub fn rules(&self) -> Vec<WafRule> {
        self.inner.read().iter().map(|c| c.rule.clone()).collect()
    }

    pub fn set_rules(&self, rules: Vec<WafRule>) {
        let compiled = rules
            .into_iter()
            .map(|rule| {
                let path_re = rule
                    .when
                    .path_regex
                    .as_ref()
                    .and_then(|p| Regex::new(p).ok());
                Compiled { rule, path_re }
            })
            .collect();
        *self.inner.write() = compiled;
    }

    pub fn add_rule(&self, rule: WafRule) {
        let path_re = rule.when.path_regex.as_ref().and_then(|p| Regex::new(p).ok());
        self.inner.write().push(Compiled { rule, path_re });
    }

    /// Evaluate a request. Custom rules run in order first (Allow short-circuits,
    /// Deny blocks); then the managed signatures run if enabled.
    pub fn evaluate(&self, ctx: &RequestCtx) -> Verdict {
        let target = format!("{} {}", ctx.path, ctx.query).to_lowercase();
        for c in self.inner.read().iter() {
            if !c.rule.enabled || !matches(c, ctx, &target) {
                continue;
            }
            match c.rule.action {
                WafAction::Allow => return Verdict::Allow,
                WafAction::Deny => {
                    return Verdict::Deny {
                        rule_id: c.rule.id.clone(),
                        reason: c.rule.description.clone(),
                    }
                }
                WafAction::Log => { /* fall through */ }
            }
        }
        if *self.managed.read() {
            let probe = format!("{} {}", ctx.path, ctx.query);
            if self.sqli.is_match(&probe) {
                return deny("managed-sqli", "SQL injection signature");
            }
            if self.xss.is_match(&probe) {
                return deny("managed-xss", "Cross-site scripting signature");
            }
            if self.traversal.is_match(&probe) {
                return deny("managed-traversal", "Path traversal signature");
            }
        }
        Verdict::Allow
    }
}

fn deny(id: &str, reason: &str) -> Verdict {
    Verdict::Deny {
        rule_id: id.to_string(),
        reason: reason.to_string(),
    }
}

fn matches(c: &Compiled, ctx: &RequestCtx, target: &str) -> bool {
    let m = &c.rule.when;
    if let Some(re) = &c.path_re {
        if !re.is_match(ctx.path) {
            return false;
        }
    }
    if let Some(method) = &m.method {
        if !method.eq_ignore_ascii_case(ctx.method) {
            return false;
        }
    }
    if let Some(prefix) = &m.ip_prefix {
        if !ctx.ip.starts_with(prefix) {
            return false;
        }
    }
    if let Some((name, val)) = &m.header_contains {
        let found = ctx.headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case(name) && v.to_lowercase().contains(&val.to_lowercase())
        });
        if !found {
            return false;
        }
    }
    if let Some(sub) = &m.contains {
        if !target.contains(&sub.to_lowercase()) {
            return false;
        }
    }
    // A rule with no conditions matches nothing (avoid accidental allow-all).
    m.path_regex.is_some()
        || m.method.is_some()
        || m.ip_prefix.is_some()
        || m.header_contains.is_some()
        || m.contains.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(path: &'a str, query: &'a str, ip: &'a str) -> RequestCtx<'a> {
        RequestCtx { method: "GET", path, query, ip, headers: &[] }
    }

    #[test]
    fn managed_blocks_sqli_and_xss() {
        let waf = Waf::new();
        assert_eq!(waf.evaluate(&ctx("/", "q=1", "1.2.3.4")), Verdict::Allow);
        assert!(matches!(
            waf.evaluate(&ctx("/search", "q=1 union select pwd", "1.2.3.4")),
            Verdict::Deny { .. }
        ));
        assert!(matches!(
            waf.evaluate(&ctx("/c", "x=<script>alert(1)</script>", "1.2.3.4")),
            Verdict::Deny { .. }
        ));
        assert!(matches!(
            waf.evaluate(&ctx("/../../etc/passwd", "", "1.2.3.4")),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn custom_deny_rule_by_ip() {
        let waf = Waf::new();
        waf.add_rule(WafRule {
            id: "block-badnet".into(),
            description: "block 10.0.0.0/8".into(),
            action: WafAction::Deny,
            enabled: true,
            when: WafMatch { ip_prefix: Some("10.0.".into()), ..Default::default() },
        });
        assert!(matches!(waf.evaluate(&ctx("/", "", "10.0.5.5")), Verdict::Deny { .. }));
        assert_eq!(waf.evaluate(&ctx("/", "", "8.8.8.8")), Verdict::Allow);
    }
}
