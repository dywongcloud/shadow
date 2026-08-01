//! Bot management — classify traffic by User-Agent and apply a policy.
//!
//! Beyond human / good-bot / bad-bot, we recognize the **three types of AI bot
//! traffic** (per Vercel's taxonomy) and let each be allowed, logged, or blocked
//! independently:
//!
//! 1. **AI crawlers** — train/index models by scraping content in bulk, not tied
//!    to any user request (GPTBot, ClaudeBot, CCBot, Google-Extended, Bytespider…).
//! 2. **AI assistants** — fetch a page in real time to answer a user's prompt,
//!    often sending referral traffic back (ChatGPT-User, OAI-SearchBot,
//!    Perplexity-User…).
//! 3. **AI agents** — act autonomously on a user's behalf (browse, click, buy);
//!    they can mutate state, so they warrant verification (Operator, ChatGPT-Agent…).

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Which of the three AI traffic types a bot belongs to.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiKind {
    Crawler,
    Assistant,
    Agent,
}

/// What to do with a class of traffic.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Let it through untouched.
    Allow,
    /// Let it through but record it for analytics.
    Log,
    /// Block with 403.
    Block,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum BotClass {
    Human,
    GoodBot { name: String },
    BadBot { name: String },
    AiBot { name: String, kind: AiKind },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BotPolicy {
    /// Allow verified good bots (search engines, etc.).
    pub allow_good: bool,
    /// Block known bad bots / scrapers.
    pub block_bad: bool,
    /// AI training/indexing crawlers.
    #[serde(default = "default_log")]
    pub ai_crawler: Action,
    /// AI assistants answering a user prompt in real time.
    #[serde(default = "default_allow")]
    pub ai_assistant: Action,
    /// Autonomous AI agents acting on a user's behalf.
    #[serde(default = "default_log")]
    pub ai_agent: Action,
}
fn default_log() -> Action {
    Action::Log
}
fn default_allow() -> Action {
    Action::Allow
}

impl Default for BotPolicy {
    fn default() -> Self {
        BotPolicy {
            allow_good: true,
            block_bad: true,
            // Sensible defaults from the article: don't block assistants (they
            // bring referral traffic); log crawlers + agents so you can decide.
            ai_crawler: Action::Log,
            ai_assistant: Action::Allow,
            ai_agent: Action::Log,
        }
    }
}

/// The outcome of evaluating a request against the bot policy.
pub enum BotVerdict {
    Allow,
    /// Allowed but noteworthy (logged) — carries a label for analytics.
    Log(String),
    Block(String),
}

pub struct BotManager {
    good: Vec<(Regex, &'static str)>,
    bad: Vec<(Regex, &'static str)>,
    ai: Vec<(Regex, &'static str, AiKind)>,
    generic_bot: Regex,
}

impl BotManager {
    pub fn new() -> BotManager {
        let g = |p: &str| Regex::new(p).unwrap();
        BotManager {
            // AI bots are matched FIRST so they aren't swept up by the generic
            // "bot|crawler" rule below.
            ai: vec![
                // 1) Crawlers (training / indexing).
                (g(r"(?i)gptbot"), "GPTBot", AiKind::Crawler),
                (g(r"(?i)oai-searchbot"), "OAI-SearchBot", AiKind::Crawler),
                (
                    g(r"(?i)(claudebot|claude-web|anthropic-ai)"),
                    "ClaudeBot",
                    AiKind::Crawler,
                ),
                (g(r"(?i)ccbot"), "CCBot", AiKind::Crawler),
                (
                    g(r"(?i)google-extended"),
                    "Google-Extended",
                    AiKind::Crawler,
                ),
                (g(r"(?i)bytespider"), "Bytespider", AiKind::Crawler),
                (g(r"(?i)amazonbot"), "Amazonbot", AiKind::Crawler),
                (
                    g(r"(?i)applebot-extended"),
                    "Applebot-Extended",
                    AiKind::Crawler,
                ),
                (
                    g(r"(?i)(meta-externalagent|facebookbot)"),
                    "Meta-ExternalAgent",
                    AiKind::Crawler,
                ),
                (g(r"(?i)(perplexitybot)"), "PerplexityBot", AiKind::Crawler),
                (
                    g(r"(?i)(cohere-ai|diffbot|omgili|youbot|imagesiftbot|dataforseobot|timpibot)"),
                    "AICrawler",
                    AiKind::Crawler,
                ),
                // 2) Assistants (real-time fetch to answer a user).
                (g(r"(?i)chatgpt-user"), "ChatGPT-User", AiKind::Assistant),
                (
                    g(r"(?i)perplexity-user"),
                    "Perplexity-User",
                    AiKind::Assistant,
                ),
                (g(r"(?i)claude-user"), "Claude-User", AiKind::Assistant),
                (g(r"(?i)gemini-user"), "Gemini-User", AiKind::Assistant),
                // 3) Autonomous agents (take actions).
                (
                    g(r"(?i)(chatgpt-agent|openai-operator|\boperator\b)"),
                    "AI-Agent",
                    AiKind::Agent,
                ),
            ],
            good: vec![
                (g(r"(?i)googlebot"), "Googlebot"),
                (g(r"(?i)bingbot"), "Bingbot"),
                (g(r"(?i)duckduckbot"), "DuckDuckBot"),
                (g(r"(?i)slackbot"), "Slackbot"),
                (g(r"(?i)applebot"), "Applebot"),
                (
                    g(r"(?i)(twitterbot|discordbot|facebookexternalhit)"),
                    "SocialPreview",
                ),
                (g(r"(?i)uptimerobot"), "UptimeRobot"),
            ],
            bad: vec![
                (
                    g(r"(?i)(semrushbot|ahrefsbot|mj12bot|dotbot|petalbot)"),
                    "SEOScraper",
                ),
                (
                    g(r"(?i)(python-requests|scrapy|go-http-client|httpclient|libwww-perl)"),
                    "Scraper",
                ),
                (g(r"(?i)(masscan|nikto|sqlmap|nmap|zgrab)"), "Scanner"),
            ],
            generic_bot: g(r"(?i)(bot|spider|crawler|crawl)"),
        }
    }

    pub fn classify(&self, user_agent: &str) -> BotClass {
        for (re, name, kind) in &self.ai {
            if re.is_match(user_agent) {
                return BotClass::AiBot {
                    name: name.to_string(),
                    kind: *kind,
                };
            }
        }
        for (re, name) in &self.good {
            if re.is_match(user_agent) {
                return BotClass::GoodBot {
                    name: name.to_string(),
                };
            }
        }
        for (re, name) in &self.bad {
            if re.is_match(user_agent) {
                return BotClass::BadBot {
                    name: name.to_string(),
                };
            }
        }
        if self.generic_bot.is_match(user_agent) {
            return BotClass::BadBot {
                name: "UnverifiedBot".to_string(),
            };
        }
        BotClass::Human
    }

    /// Full verdict (allow / log / block) for a request under `policy`.
    pub fn evaluate(&self, user_agent: &str, policy: BotPolicy) -> BotVerdict {
        match self.classify(user_agent) {
            BotClass::Human => BotVerdict::Allow,
            BotClass::GoodBot { name } => {
                if policy.allow_good {
                    BotVerdict::Allow
                } else {
                    BotVerdict::Block(format!("good bot disallowed: {name}"))
                }
            }
            BotClass::BadBot { name } => {
                if policy.block_bad {
                    BotVerdict::Block(format!("bad bot: {name}"))
                } else {
                    BotVerdict::Log(format!("bad bot (allowed): {name}"))
                }
            }
            BotClass::AiBot { name, kind } => {
                let action = match kind {
                    AiKind::Crawler => policy.ai_crawler,
                    AiKind::Assistant => policy.ai_assistant,
                    AiKind::Agent => policy.ai_agent,
                };
                let label = format!("AI {}: {name}", kind_label(kind));
                match action {
                    Action::Allow => BotVerdict::Allow,
                    Action::Log => BotVerdict::Log(label),
                    Action::Block => BotVerdict::Block(label),
                }
            }
        }
    }

    /// Backwards-compatible block check.
    pub fn should_block(&self, user_agent: &str, policy: BotPolicy) -> Option<String> {
        match self.evaluate(user_agent, policy) {
            BotVerdict::Block(r) => Some(r),
            _ => None,
        }
    }
}

fn kind_label(kind: AiKind) -> &'static str {
    match kind {
        AiKind::Crawler => "crawler",
        AiKind::Assistant => "assistant",
        AiKind::Agent => "agent",
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
        assert!(matches!(
            b.classify("Googlebot/2.1"),
            BotClass::GoodBot { .. }
        ));
        assert!(matches!(
            b.classify("python-requests/2.31"),
            BotClass::BadBot { .. }
        ));
        assert!(matches!(
            b.classify("some-random-crawler/1.0"),
            BotClass::BadBot { .. }
        ));
    }

    #[test]
    fn classifies_ai_traffic() {
        let b = BotManager::new();
        assert!(matches!(
            b.classify("Mozilla/5.0 (compatible; GPTBot/1.1)"),
            BotClass::AiBot {
                kind: AiKind::Crawler,
                ..
            }
        ));
        assert!(matches!(
            b.classify("Mozilla/5.0 ... ChatGPT-User/1.0"),
            BotClass::AiBot {
                kind: AiKind::Assistant,
                ..
            }
        ));
        assert!(matches!(
            b.classify("ClaudeBot/1.0"),
            BotClass::AiBot {
                kind: AiKind::Crawler,
                ..
            }
        ));
    }

    #[test]
    fn policy_actions_per_ai_kind() {
        let b = BotManager::new();
        let mut p = BotPolicy::default();
        // default: crawler=log (not blocked), assistant=allow.
        assert!(b.should_block("GPTBot/1.1", p).is_none());
        assert!(b.should_block("ChatGPT-User/1.0", p).is_none());
        // turn crawler blocking on.
        p.ai_crawler = Action::Block;
        assert!(b.should_block("GPTBot/1.1", p).is_some());
        assert!(b.should_block("ChatGPT-User/1.0", p).is_none());
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
