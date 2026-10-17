//! Standard `shadw.yml` project file (schema v1).
//!
//! The same file is read by the "Deploy to Shadw" GitHub Action
//! (`action.yml`) and, through this module, by the platform build itself, so a
//! push through Shadw's own GitHub integration and a deploy from Actions use the
//! same project defaults. Precedence everywhere: explicit deploy request >
//! `shadw.yml` > persisted project settings.
//!
//! Like `vercel.json`, the file is inert data: it is read once from the checkout
//! root as a bounded regular file (no symlinks), unknown keys are rejected and
//! invalid input fails the build loudly instead of being silently ignored.
//! JSON Schema: `schemas/shadw.schema.json`; docs: `docs/SHADW_YML.md`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

pub const SHADW_CONFIG_VERSION: u32 = 1;
const MAX_SHADW_CONFIG_BYTES: u64 = 64 * 1024;
const FILE_NAMES: [&str; 2] = ["shadw.yml", "shadw.yaml"];

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadwConfig {
    pub version: u32,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub root_dir: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub build: ShadwBuild,
    /// Non-secret runtime env. Applied by the GitHub Action; the platform build
    /// reports the keys but never persists values from a repository file.
    #[serde(default)]
    pub env: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub deploy: ShadwDeploy,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadwBuild {
    #[serde(default)]
    pub cache: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadwDeploy {
    #[serde(default)]
    pub wait: Option<bool>,
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
}

impl ShadwConfig {
    pub fn from_yaml(text: &str) -> anyhow::Result<ShadwConfig> {
        let config: ShadwConfig = serde_yaml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        ensure!(
            self.version == SHADW_CONFIG_VERSION,
            "shadw.yml: unsupported version {} (expected {SHADW_CONFIG_VERSION})",
            self.version
        );
        if let Some(project) = &self.project {
            ensure!(
                is_dns_label(project),
                "shadw.yml: project must be a lowercase DNS label (a-z, 0-9, -)"
            );
        }
        if let Some(root) = &self.root_dir {
            ensure!(
                !root.starts_with('/')
                    && !root.starts_with('\\')
                    && !root.split(['/', '\\']).any(|part| part == ".."),
                "shadw.yml: root_dir must be a relative path inside the repository"
            );
        }
        if let Some(target) = &self.target {
            ensure!(
                matches!(target.as_str(), "auto" | "production" | "preview"),
                "shadw.yml: target must be auto, production or preview"
            );
        }
        if let Some(minutes) = self.deploy.timeout_minutes {
            ensure!(
                (1..=120).contains(&minutes),
                "shadw.yml: deploy.timeout_minutes must be 1-120"
            );
        }
        for (key, value) in &self.env {
            ensure!(
                is_env_key(key),
                "shadw.yml: invalid env key {key:?}"
            );
            ensure!(
                !matches!(
                    value,
                    serde_yaml::Value::Mapping(_)
                        | serde_yaml::Value::Sequence(_)
                        | serde_yaml::Value::Tagged(_)
                ),
                "shadw.yml: env {key:?} must be a scalar"
            );
        }
        Ok(())
    }

    /// Root directory declared by the file; `.`/empty mean repository root.
    pub fn root_dir(&self) -> Option<&str> {
        self.root_dir
            .as_deref()
            .map(|root| root.trim().trim_matches('/'))
            .filter(|root| !root.is_empty() && *root != ".")
    }

    /// `false` only when the file explicitly disables the dependency cache.
    pub fn build_cache_enabled(&self) -> bool {
        self.build.cache != Some(false)
    }
}

fn is_dns_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

fn is_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Load `shadw.yml` (or `shadw.yaml`) from a checkout root. `Ok(None)` when
/// absent; an error when present but unsafe or invalid, or when both exist.
pub fn load_shadw_config_checked(repo: &Path) -> anyhow::Result<Option<ShadwConfig>> {
    use anyhow::{bail, ensure, Context};
    use std::io::Read;

    let mut found = None;
    for name in FILE_NAMES {
        match std::fs::symlink_metadata(repo.join(name)) {
            Ok(meta) => {
                if found.is_some() {
                    bail!("both shadw.yml and shadw.yaml exist; keep exactly one");
                }
                found = Some((name, meta));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("inspect {name}")),
        }
    }
    let Some((name, initial)) = found else {
        return Ok(None);
    };
    ensure!(
        initial.file_type().is_file() && !initial.file_type().is_symlink(),
        "{name} must be a regular file, not a symlink or directory"
    );
    ensure!(
        initial.len() <= MAX_SHADW_CONFIG_BYTES,
        "{name} exceeds the {MAX_SHADW_CONFIG_BYTES}-byte limit"
    );
    let file = std::fs::File::open(repo.join(name)).with_context(|| format!("open {name}"))?;
    let mut bytes = Vec::with_capacity(initial.len() as usize);
    file.take(MAX_SHADW_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {name}"))?;
    ensure!(
        bytes.len() as u64 <= MAX_SHADW_CONFIG_BYTES,
        "{name} grew beyond the {MAX_SHADW_CONFIG_BYTES}-byte limit while reading"
    );
    let text = std::str::from_utf8(&bytes).with_context(|| format!("{name} is not UTF-8"))?;
    let config = ShadwConfig::from_yaml(text).with_context(|| format!("parse {name}"))?;
    Ok(Some(config))
}

/// Read-only analysis wrapper; build admission uses [`load_shadw_config_checked`].
pub fn load_shadw_config(repo: &Path) -> Option<ShadwConfig> {
    load_shadw_config_checked(repo).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shadw-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_shipped_example() {
        let text = include_str!("../../../examples/shadw.yml");
        let config = ShadwConfig::from_yaml(text).expect("example must validate");
        assert_eq!(config.version, 1);
        assert_eq!(config.project.as_deref(), Some("my-app"));
        assert_eq!(config.root_dir(), None, "\".\" means repository root");
        assert!(config.build_cache_enabled());
    }

    #[test]
    fn full_file() {
        let config = ShadwConfig::from_yaml(
            "version: 1\nproject: web\nroot_dir: apps/web/\ntarget: preview\n\
             build:\n  cache: false\nenv:\n  NODE_ENV: production\n  PORT: 3000\n\
             deploy:\n  wait: false\n  timeout_minutes: 5\n",
        )
        .unwrap();
        assert_eq!(config.root_dir(), Some("apps/web"));
        assert!(!config.build_cache_enabled());
        assert_eq!(config.env.len(), 2);
    }

    #[test]
    fn rejects_invalid() {
        for text in [
            "version: 2\n",
            "project: x\n",
            "version: 1\nnope: 1\n",
            "version: 1\nproject: Bad_Name\n",
            "version: 1\nroot_dir: ../escape\n",
            "version: 1\nroot_dir: /abs\n",
            "version: 1\ntarget: prod\n",
            "version: 1\nbuild:\n  foo: true\n",
            "version: 1\nenv:\n  1BAD: x\n",
            "version: 1\nenv:\n  NESTED:\n    a: b\n",
            "version: 1\ndeploy:\n  timeout_minutes: 0\n",
        ] {
            assert!(ShadwConfig::from_yaml(text).is_err(), "accepted: {text:?}");
        }
    }

    #[test]
    fn loader_absent_present_and_conflicts() {
        let dir = tmp();
        assert!(load_shadw_config_checked(&dir).unwrap().is_none());
        std::fs::write(dir.join("shadw.yaml"), "version: 1\nroot_dir: api\n").unwrap();
        let config = load_shadw_config_checked(&dir).unwrap().unwrap();
        assert_eq!(config.root_dir(), Some("api"));
        std::fs::write(dir.join("shadw.yml"), "version: 1\n").unwrap();
        assert!(load_shadw_config_checked(&dir).is_err(), "two files is ambiguous");
        std::fs::remove_file(dir.join("shadw.yaml")).unwrap();
        std::fs::write(dir.join("shadw.yml"), "version: 1\nbogus: 1\n").unwrap();
        assert!(load_shadw_config_checked(&dir).is_err());
        assert!(load_shadw_config(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn loader_refuses_symlink() {
        let dir = tmp();
        std::fs::write(dir.join("real.yml"), "version: 1\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real.yml"), dir.join("shadw.yml")).unwrap();
        assert!(load_shadw_config_checked(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
