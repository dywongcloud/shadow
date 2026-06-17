//! Bot management — classify traffic by User-Agent into human / verified good
//! bot / bad bot, and apply a policy (allow good crawlers, block bad ones).

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum BotClass {
    Human,
    GoodBot { name: String },
    BadBot { name: String },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BotPolicy {
    /// Allow verified good bots (search engines, etc.).
    pub allow_good: bool,
    /// Block known bad bots / scrapers.
    pub block_bad: bool,
}

impl Default for BotPolicy {
    fn default() -> Self {
        BotPolicy { allow_good: true, block_bad: true }
    }
}

pub struct BotManager {
    good: Vec<(Regex, &'static str)>,
    bad: Vec<(Regex, &'static str)>,
    generic_bot: Regex,
}

impl BotManager {
    pub fn new() -> BotManager {
        let g = |p: &str| Regex::new(p).unwrap();
        BotManager {
            good: vec![
                (g(r"(?i)googlebot"), "Googlebot"),
                (g(r"(?i)bingbot"), "Bingbot"),
                (g(r"(?i)duckduckbot"), "DuckDuckBot"),
                (g(r"(?i)slackbot"), "Slackbot"),
                (g(r"(?i)applebot"), "Applebot"),
                (g(r"(?i)(twitterbot|discordbot|facebookexternalhit)"), "SocialPreview"),
                (g(r"(?i)uptimerobot"), "UptimeRobot"),
            ],
            bad: vec![
                (g(r"(?i)(semrushbot|ahrefsbot|mj12bot|dotbot|petalbot)"), "SEOScraper"),
                (g(r"(?i)(python-requests|scrapy|go-http-client|httpclient|libwww-perl)"), "Scraper"),
                (g(r"(?i)(masscan|nikto|sqlmap|nmap|zgrab)"), "Scanner"),
            ],
            generic_bot: g(r"(?i)(bot|spider|crawler|crawl)"),
        }
    }

    pub fn classify(&self, user_agent: &str) -> BotClass {
        for (re, name) in &self.good {
            if re.is_match(user_agent) {
                return BotClass::GoodBot { name: name.to_string() };
            }
        }
        for (re, name) in &self.bad {
            if re.is_match(user_agent) {
                return BotClass::BadBot { name: name.to_string() };
            }
        }
        if self.generic_bot.is_match(user_agent) {
            return BotClass::BadBot { name: "UnverifiedBot".to_string() };
        }
        BotClass::Human
    }

    /// Returns Some(reason) if the request should be blocked under `policy`.
    pub fn should_block(&self, user_agent: &str, policy: BotPolicy) -> Option<String> {
        match self.classify(user_agent) {
            BotClass::BadBot { name } if policy.block_bad => Some(format!("bad bot: {name}")),
            _ => None,
        }
    }
}

impl Default for BotManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_bots() {
        let b = BotManager::new();
        assert_eq!(b.classify("Mozilla/5.0 ... Chrome/120"), BotClass::Human);
        assert!(matches!(b.classify("Googlebot/2.1"), BotClass::GoodBot { .. }));
        assert!(matches!(b.classify("python-requests/2.31"), BotClass::BadBot { .. }));
        assert!(matches!(b.classify("some-random-crawler/1.0"), BotClass::BadBot { .. }));
    }

    #[test]
    fn policy_blocks_bad_only() {
        let b = BotManager::new();
        let p = BotPolicy::default();
        assert!(b.should_block("sqlmap/1.5", p).is_some());
        assert!(b.should_block("Googlebot/2.1", p).is_none());
        assert!(b.should_block("Mozilla/5.0", p).is_none());
    }
}
