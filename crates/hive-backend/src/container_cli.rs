//! OS-aware container CLI abstraction: Apple's `container` tool
//! (<https://opensource.apple.com/projects/container/>, Virtualization.framework
//! microVMs, one VM per container — no shared Linux VM daemon to hang) on
//! macOS, `podman` everywhere else (the real fleet nodes are Linux, where
//! podman is mature and already correct).
//!
//! The two CLIs are OCI/Docker-shaped but NOT flag-compatible in several real
//! ways this module hides from callers:
//! - remove a container: podman `rm -f`, container `delete -f`
//! - tail logs: podman `logs --tail N`, container `logs -n N`
//! - list volumes: podman supports `--filter`, container does not (both
//!   support `--format json`, with DIFFERENT field shapes — unified here)
//! - resource ceilings: container has no `--pids-limit`/`--security-opt`
//!   (each container already runs in its own lightweight VM — materially
//!   STRONGER isolation than a podman namespace tweak, so dropping these on
//!   macOS is not a security regression)
//! - sandbox runtime (podman `--runtime runsc`/gVisor): meaningless for
//!   `container` — every container is already VM-isolated, so callers should
//!   skip the runtime-fallback dance entirely when using `container`
//! - pull: podman accepts bare `pull`, container requires `image pull`
//! - `--ip`/`--add-host` (static IP + sibling `/etc/hosts` for multi-service
//!   compose networks): container has NEITHER. NOT handled transparently —
//!   multi-service/compose deploys stay on podman even on macOS; see the
//!   callers in `lib.rs::podman_run_container` and `mock.rs` for the explicit
//!   fallback, and `assigned_ipv4`/`inject_hosts` below for the workaround
//!   primitives a future compose-aware caller can build on.
//!
//! Every function here takes an explicit `apple: bool` rather than reading the
//! host OS implicitly — callers that mix backends in one code path (like the
//! multi-service exception above, which stays on podman even when running on
//! macOS) are the single source of truth for which CLI a given call targets.
//! `is_apple_default()` is the OS-default a caller starts from before applying
//! its own overrides.

use std::process::Stdio;
use tokio::process::Command;

use crate::ContainerLimits;

/// Whether THIS OS defaults to Apple's `container` tool. Callers with no
/// reason to override start from this; callers that must force one CLI or the
/// other (e.g. multi-service compose, always podman) pass their own bool to
/// every function below instead of calling this per-operation.
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

/// Does this deploy's `net_json` (the `start_cmd[3]` run-config every container
/// deploy carries — see `git.rs::container_volume_cfg`) actually need podman's
/// `--ip`/`--add-host` (a real STATIC IP for this container, and/or hardcoded
/// sibling `/etc/hosts` entries)? Every container gets attached to a
/// project-scoped network for TENANT isolation (one tenant's container must
/// never reach another's on a shared default bridge) — that part is universal
/// and works fine on Apple's `container` tool too (it has its own per-network
/// isolation). Only a real multi-service COMPOSE deploy pins a static `ip` and/or
/// ships non-empty `hosts` entries, and only THAT case has no `container`
/// equivalent (see the module doc). Use this, not `net_json.is_some()`, to
/// decide whether a deploy is Apple-`container`-eligible — `net_json` being
/// present at all is the NORMAL case (it also carries the automatic persistent
/// volume config), so checking bare presence would wrongly force EVERY deploy
/// onto podman.
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

/// Args to remove a container by name (best-effort teardown).
///
/// podman gets `-v`, which removes the container's ANONYMOUS volumes along with
/// it. Without it every container that ran an image declaring `VOLUME` left its
/// anonymous volume behind forever — and podman allocates one lock per VOLUME as
/// well as per container out of a single `num_locks` pool (default 2048). Live
/// on the leader that pool was fully consumed by 2032 volumes against just 16
/// containers, so `podman run` failed with "allocating lock for new container:
/// exceeded num_locks (2048)" and every cold start 503'd as CAPACITY_EXHAUSTED.
///
/// `-v` is specifically safe: it removes only anonymous volumes, never NAMED
/// ones — so a provisioned database volume (`hive-vol-postgres`) is untouched.
/// That is why this is the right place to fix the leak, rather than pruning
/// unused volumes later (a blanket prune WOULD delete named DB volumes).
/// Apple's `container delete` has no `-v` equivalent, so it is unchanged.
pub fn rm_args(apple: bool, name: &str) -> Vec<String> {
    if apple {
        vec!["delete".into(), "-f".into(), name.into()]
    } else {
        vec!["rm".into(), "-f".into(), "-v".into(), name.into()]
    }
}

/// Args to remove a volume by name. `container volume delete` has no force
/// flag (only `--all`) — dropped on macOS; the call is always best-effort
/// (`let _ =`) at every existing call site regardless.
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

/// Per-container resource-ceiling flags for `run`. `--memory`/`--cpus` are
/// supported by both CLIs; `--pids-limit`/`--security-opt no-new-privileges`
/// are podman-only (see module doc — a VM boundary already covers the same
/// threat model on macOS).
pub fn resource_flags(apple: bool, limits: &ContainerLimits) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(mem) = &limits.memory {
        f.push("--memory".into());
        f.push(mem.clone());
    }
    if let Some(cpus) = &limits.cpus {
        f.push("--cpus".into());
        f.push(if apple {
            // podman accepts a fractional CPU quota ("2.0", "0.5"); Apple's
            // `container --cpus` requires a plain integer (verified live:
            // "2.0" is rejected as invalid) and has no fractional-core
            // concept. Round to the nearest whole core, minimum 1.
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
        // `container inspect` has no Go-template `-f` filter (podman-only) —
        // parse the JSON `status.state` field instead.
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

/// All volume names currently known to the backend. Unifies podman's
/// capitalized top-level `Name` and container's nested lowercase
/// `configuration.name` JSON shapes behind one call so callers never touch
/// either CLI's raw `--format`/`--filter` differences directly.
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

/// Compatibility wrapper for best-effort callers. Destructive ownership
/// cleanup uses [`try_list_volume_names`] so an enumeration failure can never
/// masquerade as an empty backend.
pub async fn list_volume_names(apple: bool, path_env: &str) -> Vec<String> {
    try_list_volume_names(apple, path_env)
        .await
        .unwrap_or_default()
}

/// Is the given container CLI installed and responsive? (`bin --version`).
pub async fn available(apple: bool) -> bool {
    // `--version` only proves the CLI binary exists; podman's client can still
    // be unable to reach its service VM (witnessed: one malformed known_hosts
    // line broke the socket, `--version` succeeded, and every volume op failed
    // silently — turning an environmental fault into what looked like a data
    // bug). A real round-trip (`volume ls`) is the honest availability signal.
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

/// Query a running container's assigned IPv4 address on a named network
/// (Apple's `container` tool only — used solely by a macOS multi-service
/// sibling-hosts workaround; podman's `--ip`/`--add-host` cover this natively
/// and never need this). Returns the bare address (no `/prefix`).
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

/// Inject sibling `/etc/hosts` entries into an already-running container via
/// `exec` (Apple's `container` tool only — the macOS equivalent of podman's
/// `--add-host`, which `container run` has no counterpart for). Best-effort;
/// requires a POSIX shell in the target image (true for virtually all real
/// app images; a from-scratch/distroless image has no shell and is a known,
/// accepted limitation of this workaround, matching the same limitation any
/// exec-based approach has).
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
