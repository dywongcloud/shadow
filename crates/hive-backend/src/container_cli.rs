//! OS-aware container CLI abstraction: Apple's `container` tool (Virtualization.framework, one VM
//! per container) on macOS, `podman` everywhere else. The flag differences are hidden here (`rm -f`
//! vs `delete -f`, `logs --tail` vs `-n`, volume listing shapes, podman-only `--pids-limit` /
//! `--security-opt` / `--runtime`, `image pull`); `--ip`/`--add-host` have NO `container`
//! equivalent, so multi-service compose deploys stay on podman even on macOS. Every function
//! takes an explicit `apple: bool`; `is_apple_default()` is only the starting point.

use std::process::Stdio;
use tokio::process::Command;

use crate::ContainerLimits;

/// Whether THIS OS defaults to Apple's `container` tool; callers that must force one CLI pass
/// their own bool to every function instead.
pub fn is_apple_default() -> bool {
    cfg!(target_os = "macos")
}

/// The container CLI binary name for a given `apple` choice.
pub fn bin(apple: bool) -> &'static str {
    if apple {
        "container"
    } else {
        "podman"
    }
}

/// Does this deploy's `net_json` actually need podman's `--ip`/`--add-host` (a static ip and/or
/// sibling `hosts`)? Only a real multi-service compose deploy does; bare presence of `net_json` is
/// the NORMAL case (it also carries the volume config) and must not force podman.
pub fn needs_podman_networking(net: &serde_json::Value) -> bool {
    let has_ip = net
        .get("ip")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_hosts = net
        .get("hosts")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    has_ip || has_hosts
}

/// Args to remove a container by name. podman gets `-v` so its ANONYMOUS volumes die with it
/// (each holds a lock from the shared `num_locks` pool); named volumes are untouched. Apple's
/// `container delete` has no `-v`.
pub fn rm_args(apple: bool, name: &str) -> Vec<String> {
    if apple {
        vec!["delete".into(), "-f".into(), name.into()]
    } else {
        vec!["rm".into(), "-f".into(), "-v".into(), name.into()]
    }
}

/// Args to remove a volume by name; `container volume delete` has no force flag.
pub fn volume_rm_args(apple: bool, name: &str) -> Vec<String> {
    if apple {
        vec!["volume".into(), "rm".into(), name.into()]
    } else {
        vec!["volume".into(), "rm".into(), "-f".into(), name.into()]
    }
}

/// Args to tail the last `n` log lines.
pub fn logs_tail_args(apple: bool, name: &str, n: u32) -> Vec<String> {
    if apple {
        vec!["logs".into(), "-n".into(), n.to_string(), name.into()]
    } else {
        vec!["logs".into(), "--tail".into(), n.to_string(), name.into()]
    }
}

/// Args to pull an image (podman: bare `pull`; container: `image pull`).
pub fn pull_args(apple: bool, image: &str) -> Vec<String> {
    if apple {
        vec!["image".into(), "pull".into(), image.into()]
    } else {
        vec!["pull".into(), image.into()]
    }
}

/// Per-container resource-ceiling flags for `run`; `--pids-limit`/`--security-opt` are podman-only
/// (a VM boundary covers the same threat model on macOS).
pub fn resource_flags(apple: bool, limits: &ContainerLimits) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(mem) = &limits.memory {
        f.push("--memory".into());
        f.push(mem.clone());
    }
    if let Some(cpus) = &limits.cpus {
        f.push("--cpus".into());
        f.push(if apple {
            let n: f64 = cpus.parse().unwrap_or(1.0);
            n.round().max(1.0).to_string()
        } else {
            cpus.clone()
        });
    }
    if !apple {
        if let Some(pids) = &limits.pids {
            f.push("--pids-limit".into());
            f.push(pids.to_string());
        }
        f.push("--security-opt".into());
        f.push("no-new-privileges".into());
    }
    f
}

/// Is a container currently running? Best-effort: any failure (CLI missing,
/// container gone, malformed output) is treated as "not running".
pub async fn is_running(apple: bool, name: &str, path_env: &str) -> bool {
    if apple {
        let Ok(out) = Command::new(bin(apple))
            .args(["inspect", name])
            .env("PATH", path_env)
            .output()
            .await
        else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            return false;
        };
        v.as_array()
            .and_then(|a| a.first())
            .and_then(|c| c.get("status"))
            .and_then(|s| s.get("state"))
            .and_then(|s| s.as_str())
            .map(|s| s.eq_ignore_ascii_case("running"))
            .unwrap_or(false)
    } else {
        Command::new(bin(apple))
            .args(["inspect", "-f", "{{.State.Running}}", name])
            .env("PATH", path_env)
            .output()
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false)
    }
}

/// All volume names the backend knows, unifying podman's `Name` and container's
/// `configuration.name` JSON shapes.
pub async fn try_list_volume_names(apple: bool, path_env: &str) -> Result<Vec<String>, String> {
    let out = Command::new(bin(apple))
        .args(["volume", "ls", "--format", "json"])
        .env("PATH", path_env)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("{} volume ls could not start: {error}", bin(apple)))?;
    if !out.status.success() {
        return Err(format!(
            "{} volume ls exited {}: {}",
            bin(apple),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let value = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .map_err(|error| format!("{} volume ls returned invalid JSON: {error}", bin(apple)))?;
    let entries = value
        .as_array()
        .ok_or_else(|| format!("{} volume ls returned non-array JSON", bin(apple)))?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("Name")
                .or_else(|| entry.get("configuration").and_then(|c| c.get("name")))
                .or_else(|| entry.get("name"))
                .and_then(|name| name.as_str())
                .map(str::to_string)
        })
        .collect())
}

/// Compatibility wrapper for best-effort callers; destructive cleanup uses
/// [`try_list_volume_names`] so an enumeration failure never reads as an empty backend.
pub async fn list_volume_names(apple: bool, path_env: &str) -> Vec<String> {
    try_list_volume_names(apple, path_env)
        .await
        .unwrap_or_default()
}

/// Is the given container CLI installed and responsive? (`bin --version`).
pub async fn available(apple: bool) -> bool {
    let version_ok = Command::new(bin(apple))
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !version_ok {
        return false;
    }
    Command::new(bin(apple))
        .args(["volume", "ls"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A running container's assigned IPv4 on a named network (Apple `container` only — the macOS
/// multi-service sibling-hosts workaround); bare address, no prefix.
pub async fn assigned_ipv4(name: &str, path_env: &str) -> Option<String> {
    let out = Command::new(bin(true))
        .args(["inspect", name])
        .env("PATH", path_env)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let ip = v
        .as_array()?
        .first()?
        .get("status")?
        .get("networks")?
        .as_array()?
        .first()?
        .get("ipv4Address")?
        .as_str()?;
    Some(ip.split('/').next().unwrap_or(ip).to_string())
}

/// Inject sibling `/etc/hosts` entries into a running container via `exec` (Apple `container`'s
/// stand-in for `--add-host`); best-effort, needs a POSIX shell in the image.
pub async fn inject_hosts(name: &str, path_env: &str, entries: &[(String, String)]) {
    if entries.is_empty() {
        return;
    }
    let script = entries
        .iter()
        .map(|(host, ip)| format!("echo '{ip} {host}' >> /etc/hosts"))
        .collect::<Vec<_>>()
        .join(" && ");
    let _ = Command::new(bin(true))
        .args(["exec", name, "sh", "-c", &script])
        .env("PATH", path_env)
        .output()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_args_use_the_right_verb_per_cli() {
        assert_eq!(rm_args(true, "hive-abc"), vec!["delete", "-f", "hive-abc"]);
        // podman MUST carry `-v` so the container's anonymous volumes die with
        // it — omitting it leaked one podman lock per removed container until
        // the whole `num_locks` pool was gone and no container could start.
        assert_eq!(
            rm_args(false, "hive-abc"),
            vec!["rm", "-f", "-v", "hive-abc"]
        );
    }

    #[test]
    fn logs_tail_args_use_the_right_flag_per_cli() {
        assert_eq!(
            logs_tail_args(true, "hive-abc", 20),
            vec!["logs", "-n", "20", "hive-abc"]
        );
        assert_eq!(
            logs_tail_args(false, "hive-abc", 20),
            vec!["logs", "--tail", "20", "hive-abc"]
        );
    }

    #[test]
    fn pull_args_use_the_right_shape_per_cli() {
        assert_eq!(
            pull_args(true, "alpine:latest"),
            vec!["image", "pull", "alpine:latest"]
        );
        assert_eq!(
            pull_args(false, "alpine:latest"),
            vec!["pull", "alpine:latest"]
        );
    }

    #[test]
    fn volume_rm_args_drop_force_flag_on_apple_only() {
        assert_eq!(volume_rm_args(true, "v"), vec!["volume", "rm", "v"]);
        assert_eq!(volume_rm_args(false, "v"), vec!["volume", "rm", "-f", "v"]);
    }

    #[test]
    fn resource_flags_drop_podman_only_flags_on_apple() {
        let limits = ContainerLimits {
            memory: Some("512m".into()),
            cpus: Some("1.0".into()),
            pids: Some(256),
        };
        let apple_flags = resource_flags(true, &limits);
        assert!(apple_flags.contains(&"--memory".to_string()));
        assert!(apple_flags.contains(&"--cpus".to_string()));
        assert!(
            !apple_flags.contains(&"--pids-limit".to_string()),
            "container has no --pids-limit"
        );
        assert!(
            !apple_flags.contains(&"--security-opt".to_string()),
            "container has no --security-opt"
        );

        let podman_flags = resource_flags(false, &limits);
        assert!(podman_flags.contains(&"--pids-limit".to_string()));
        assert!(podman_flags.contains(&"--security-opt".to_string()));
    }

    #[test]
    fn apple_cpus_flag_is_rounded_to_a_plain_integer() {
        // REGRESSION TEST for a real, confirmed bug caught by the live
        // container-boot test: podman's fractional CPU quota format
        // ("2.0", the actual default from ContainerLimits::default()) is
        // REJECTED by Apple's `container --cpus` ("Error: The value '2.0' is
        // invalid for '--cpus <cpus>'") — it only accepts a plain integer.
        for (input, expect_apple) in [("2.0", "2"), ("0.5", "1"), ("1", "1"), ("3.6", "4")] {
            let limits = ContainerLimits {
                memory: None,
                cpus: Some(input.into()),
                pids: None,
            };
            let apple = resource_flags(true, &limits);
            let i = apple.iter().position(|f| f == "--cpus").unwrap();
            assert_eq!(apple[i + 1], expect_apple, "input {input}");

            // podman keeps the original fractional value untouched.
            let podman = resource_flags(false, &limits);
            let i = podman.iter().position(|f| f == "--cpus").unwrap();
            assert_eq!(podman[i + 1], input);
        }
    }

    #[test]
    fn needs_podman_networking_distinguishes_static_ip_from_plain_volume_config() {
        // REGRESSION TEST for a real, confirmed bug caught by a live end-to-end
        // deploy: `container_volume_cfg` (git.rs) attaches EVERY container
        // deploy — including a plain standalone one — to a project-scoped
        // network for tenant isolation (net/subnet/gw always present), with a
        // static `ip` and `hosts` ONLY for real multi-service compose. The
        // first version of this check used `net_json.is_some()` to decide
        // podman-vs-apple, which is ALWAYS true (volume config always exists)
        // — so EVERY deploy was silently forced onto podman, defeating the
        // whole macOS swap. `needs_podman_networking` must key off the
        // ip/hosts fields specifically, not bare presence of the JSON object.
        let standalone_with_volume_and_tenant_net = serde_json::json!({
            "vol": "hive-vol-app", "volpath": "/data",
            "net": "hive-net-app", "subnet": "10.10.5.0/24", "gw": "10.10.5.1",
        });
        assert!(
            !needs_podman_networking(&standalone_with_volume_and_tenant_net),
            "a standalone deploy's tenant-isolation network (no static ip/hosts) must be apple-container-eligible"
        );

        let compose_with_static_ip = serde_json::json!({
            "vol": "hive-vol-app-web", "volpath": "/data",
            "net": "hive-net-app", "subnet": "10.10.5.0/24", "gw": "10.10.5.1",
            "ip": "10.10.5.11",
        });
        assert!(
            needs_podman_networking(&compose_with_static_ip),
            "a static IP means real compose networking"
        );

        let compose_with_hosts = serde_json::json!({
            "net": "hive-net-app", "hosts": ["worker:10.10.5.12"],
        });
        assert!(
            needs_podman_networking(&compose_with_hosts),
            "non-empty hosts means real compose networking"
        );

        let empty_hosts_array = serde_json::json!({ "net": "hive-net-app", "hosts": [] });
        assert!(
            !needs_podman_networking(&empty_hosts_array),
            "an empty hosts array is not real compose networking"
        );
    }

    #[test]
    fn list_volume_names_json_shapes_both_parse() {
        // Real parsing runs through a live process in `list_volume_names`; this
        // confirms the two known on-the-wire shapes both extract a name via the
        // same lookup chain used there (Name -> configuration.name -> name).
        let podman_shape = serde_json::json!([{"Name": "vol-a"}]);
        let apple_shape = serde_json::json!([{"configuration": {"name": "vol-b"}, "id": "vol-b"}]);
        for (shape, expect) in [(podman_shape, "vol-a"), (apple_shape, "vol-b")] {
            let name = shape.as_array().unwrap()[0]
                .get("Name")
                .or_else(|| {
                    shape.as_array().unwrap()[0]
                        .get("configuration")
                        .and_then(|c| c.get("name"))
                })
                .or_else(|| shape.as_array().unwrap()[0].get("name"))
                .and_then(|n| n.as_str());
            assert_eq!(name, Some(expect));
        }
    }
}
