//! Operator diagnostic (same family as `--dns-probe` / `--swarm-probe`):
//! prove the DEFAULT Node.js LTS this host resolves for builds and deployments.
//! Binds no port, joins no mesh, starts no node, mutates no state. A host that
//! cannot prove the default is NOT CAPABLE: non-zero exit, never a vacuous PASS.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Result};

pub async fn run() -> Result<()> {
    let major = fluid_core::DEFAULT_NODE_MAJOR;
    let version = fluid_core::DEFAULT_NODE_VERSION;
    let build_output = fluid_core::DEFAULT_BUILD_OUTPUT_NODE_RUNTIME;
    println!("RESULT default major={major} version={version} build_output={build_output}");

    let good_dir = format!("v{version}");
    let other_major = format!("v{}.22.2", major - 1);
    let bad_dirs = [
        format!("v{version}.evil"),
        format!("v{version}-rc"),
        format!("v{version}.1"),
        format!("{version}"),
        format!("v{major}"),
        other_major,
    ];
    let mut dir_accepted = 0;
    let mut dir_refused = 0;
    if crate::git::strict_lts_dir(&good_dir).is_some() {
        dir_accepted += 1;
    } else {
        bail!("the pinned default version directory was refused: {good_dir}");
    }
    for candidate in &bad_dirs {
        if crate::git::strict_lts_dir(candidate).is_none() {
            dir_refused += 1;
        } else {
            bail!("a non-semantic or other-major directory was admitted: {candidate}");
        }
    }
    println!("RESULT strict_dir=PASS accepted={dir_accepted} refused={dir_refused}");

    let good_version = format!("v{version}");
    let bad_versions = [
        String::new(),
        "unavailable".to_string(),
        format!("{version}"),
        format!("v{version}-rc.1"),
        format!("v{version}.1"),
        "v24.x.0".to_string(),
    ];
    let mut version_accepted = 0;
    let mut version_refused = 0;
    if crate::git::node_version_major(&good_version) == Some(major) {
        version_accepted += 1;
    } else {
        bail!("the pinned default version did not parse to major {major}");
    }
    for candidate in &bad_versions {
        if crate::git::node_version_major(candidate).is_none() {
            version_refused += 1;
        } else {
            bail!("a non-semantic version string parsed: {candidate:?}");
        }
    }
    println!("RESULT version_parse=PASS accepted={version_accepted} refused={version_refused}");

    let Some(bin_dir) = crate::git::preferred_node_bin() else {
        println!("RESULT probed_bin=NOT_CAPABLE typed_refusal=NODE_RUNTIME_MISSING");
        bail!("default Node.js LTS is not provisioned on this host");
    };
    let node = PathBuf::from(&bin_dir).join("node");
    let output = Command::new(&node).arg("--version").output()?;
    if !output.status.success() {
        bail!("probed node --version did not succeed");
    }
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let reported_major = crate::git::node_version_major(&reported)
        .ok_or_else(|| anyhow::anyhow!("probed node printed a non-semantic version: {reported:?}"))?;
    println!("RESULT executed=PASS reported={reported} major={reported_major}");
    if reported_major != major {
        bail!("probed node reports major {reported_major}, not the default {major}");
    }

    let identity = crate::git::build_toolchain_identity(std::env::temp_dir().as_path(), "npm").await;
    let verified = identity.contains("verified=1");
    let overridden = std::env::var("HIVE_BUILD_TOOLCHAIN_ID")
        .map(|raw| !raw.trim().is_empty())
        .unwrap_or(false);
    println!("RESULT cache_identity=PASS verified={verified} operator_override={overridden} identity={identity}");
    if !verified && !overridden {
        bail!("cache identity refused to claim the default major without an operator override");
    }

    let sqlite = Command::new(&node)
        .args(["-e", "require(\"node:sqlite\")"])
        .output()?;
    println!("RESULT node_sqlite={}", if sqlite.status.success() { "PASS" } else { "NOT_CAPABLE" });
    println!("NODE_PROBE=PASS");
    Ok(())
}
