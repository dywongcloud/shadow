//! Memory observability and an episodic-burst catcher. OOM kills here are BURSTS (a process
//! consuming everything the host has within minutes), so profiling the idle state never describes
//! them. Two things are provided:
//!
//! 1. **The numbers that distinguish a leak from fragmentation.** [`snapshot`] reads jemalloc's own
//! `stats.allocated/active/resident/retained/mapped` live through `GET /v1/debug/memory`
//! (`stats_print` writes only at exit, which a node being killed never reaches). A sampled live
//! heap far below RSS says most resident memory is not live objects.
//!
//! 2. **A profile taken DURING the burst.** Sampling is off at boot on purpose, so the watchdog
//! ([`spawn`]) arms it when RSS crosses a low threshold and dumps when it crosses a high one: the
//! next burst leaves a profile on disk instead of only a kernel kill line.
//!
//! Arming is OPT-IN (`HIVE_MEM_PROF=1`), never automatic: jemalloc captures a backtrace per sampled
//! allocation with `prof_backtrace_impl`, a frame-pointer stack walk, and a release build without
//! frame pointers hands it a garbage saved-rbp chain and it faults (SIGSEGV seconds after arming),
//! so arming killed the node it existed to diagnose.
//!
//! Both halves are bounded and honest: dumps are rate-limited, capped in count and written under
//! `$HIVE_DATA` (never `/tmp`, which a reboot wipes); arming is logged at WARN naming the threshold
//! that fired, so a profile appearing on disk is never a mystery.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Set once the watchdog has turned jemalloc sampling on, so it is armed at
/// most once per process.
static ARMED: AtomicBool = AtomicBool::new(false);
/// epoch-ms of the last profile dump (rate limit).
static LAST_DUMP_MS: AtomicU64 = AtomicU64::new(0);
/// Dumps written this process — reported so an operator can tell "never fired"
/// from "fired and the files were reaped".
static DUMPS: AtomicU64 = AtomicU64::new(0);
/// Logged once that profiling was SKIPPED, so a dump with no attribution is
/// never a mystery.
static PROF_SKIP_WARNED: AtomicBool = AtomicBool::new(false);

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// Reads one `Key: <n> kB` field of `/proc/self/status`, in bytes. `VmRSS` rather than
/// `/proc/self/statm`: statm counts PAGES and the page size differs by architecture (4 KiB on the
/// x86_64 fleet, 16/64 KiB on aarch64), a silent 16x error in exactly the number the watchdog
/// thresholds compare against. The kernel already did the conversion here.
fn proc_status_kb(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            let n: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(n.saturating_mul(1024));
        }
    }
    None
}

/// This process's resident set in bytes. `None` off Linux or when `/proc` is unreadable, never a
/// guess: a fabricated RSS would arm the watchdog wrongly or never arm it.
pub fn rss_bytes() -> Option<u64> {
    proc_status_kb("VmRSS:")
}

/// Live threads in this process (`/proc/self/status: Threads`). jemalloc's per-arena dirty pages
/// scale with thread count, so it is the second input to the fragmentation reading.
fn thread_count() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Threads:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// jemalloc's own accounting. `epoch` MUST be advanced first: every `stats.*` mallctl reads a
/// snapshot that refreshes only on an epoch write, so skipping it returns the same numbers forever,
/// indistinguishable from stable memory. `allocated` is live bytes (the leak number); `resident -
/// allocated` is the fragmentation/dirty-page gap; `retained` is virtual memory returned to the OS
/// but not unmapped, so NOT resident.
#[cfg(target_os = "linux")]
fn jemalloc_stats() -> Value {
    let _ = unsafe { tikv_jemalloc_ctl::raw::write::<u64>(b"epoch\0", 1) };
    let read = |name: &[u8]| -> Option<u64> {
        unsafe { tikv_jemalloc_ctl::raw::read::<usize>(name) }
            .ok()
            .map(|v| v as u64)
    };
    let prof_active = unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0") }.ok();
    json!({
        "allocated": read(b"stats.allocated\0"),
        "active": read(b"stats.active\0"),
        "resident": read(b"stats.resident\0"),
        "retained": read(b"stats.retained\0"),
        "mapped": read(b"stats.mapped\0"),
        "metadata": read(b"stats.metadata\0"),
        "prof_active": prof_active,
    })
}

#[cfg(not(target_os = "linux"))]
fn jemalloc_stats() -> Value {
    json!({ "unavailable": "jemalloc stats are Linux-only on this build" })
}

/// One memory reading: OS view, allocator view, and the live gauges for every
/// bound this crate enforces on tenant-driven growth. Backing
/// `GET /v1/debug/memory` and the watchdog's own log line.
pub fn snapshot(hive: Option<&hive_controlplane::Hive>) -> Value {
    let (log_lines, log_truncated, log_dropped_bytes) = hive_core::LOG_CAP_STATS.snapshot();
    let (job_records, job_log_buses, job_log_bytes) = match hive {
        Some(h) => {
            let (jobs, buses) = h.retained_job_counts();
            (Some(jobs), Some(buses), Some(h.log_retention_bytes()))
        }
        None => (None, None, None),
    };
    json!({
        "rss_bytes": rss_bytes(),
        "threads": thread_count(),
        "jemalloc": jemalloc_stats(),
        "bounds": {
            "log_lines_captured": log_lines,
            "log_lines_truncated": log_truncated,
            "log_bytes_dropped": log_dropped_bytes,
            "log_line_cap_bytes": hive_core::MAX_LOG_LINE_BYTES,
            "job_records_retained": job_records,
            "job_log_buses_retained": job_log_buses,
            "job_log_bytes_retained": job_log_bytes,
        },
        "profile_dumps_written": DUMPS.load(Ordering::Relaxed),
        "profiling_armed_by_watchdog": ARMED.load(Ordering::Relaxed),
    })
}

/// Directory heap dumps land in. Under `$HIVE_DATA` so a burst captured just
/// before a reboot-inducing OOM survives the reboot.
fn dump_dir() -> std::path::PathBuf {
    crate::persist::data_dir().join("heapdumps")
}

/// Keeps only the newest `keep` dumps and deletes only what this module wrote (`heap-*` files in
/// its own directory), never anything else.
fn prune_dumps(keep: usize) {
    let dir = dump_dir();
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = match std::fs::read_dir(&dir)
    {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("heap-"))
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some((m.modified().ok()?, e.path()))
            })
            .collect(),
        Err(_) => return,
    };
    if files.len() <= keep {
        return;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// Watchdog loop. Runs on EVERY node (the resource is per-host), not leader-only, and ends on its
/// first tick where `/proc` is unavailable (not Linux).
///
/// Three tiers, each tested against its OWN threshold (MiB, env-tunable), so raising one never
/// disables another:
/// * `HIVE_MEM_ARM_MB` (3072): log the full [`snapshot`] every tick, the record that survives a
///   kill (the kernel's OOM line carries one number and no attribution), and, only with
///   `HIVE_MEM_PROF=1`, turn jemalloc sampling on. Above the idle band so a normal node never
///   pays for it, far below the kill point so there is time to sample.
/// * `HIVE_MEM_DUMP_MB` (8192): write a heap profile, at most once per
///   `HIVE_MEM_DUMP_COOLDOWN_SECS` (120), keeping the newest `HIVE_MEM_DUMP_KEEP` (8). Without
///   `HIVE_MEM_PROF=1` the dump carries no per-site attribution.
/// * `HIVE_MEM_RESTART_MB` (16384): dump a final profile, then request a controlled restart
///   (`Restart=always` makes the clean exit a full recovery). Above the dump threshold so the
///   evidence exists before the exit that would erase it; a node at this band stops answering
///   gossip promptly and peers mark it isolated. `0` disables only this tier.
///
/// `HIVE_MEM_WATCH_SECS` (15) is the tick, short relative to a burst (minutes) or the watchdog
/// samples only the corpse. `HIVE_MEM_WATCH=0` disables the watchdog entirely.
pub fn spawn(hive: std::sync::Arc<hive_controlplane::Hive>, restart: crate::ControlledRestart) {
    if std::env::var("HIVE_MEM_WATCH").ok().as_deref() == Some("0") {
        tracing::info!("memory watchdog disabled (HIVE_MEM_WATCH=0)");
        return;
    }
    let interval = std::time::Duration::from_secs(env_u64("HIVE_MEM_WATCH_SECS", 15));
    let arm_bytes = env_u64("HIVE_MEM_ARM_MB", 3072) * 1024 * 1024;
    let dump_bytes = env_u64("HIVE_MEM_DUMP_MB", 8192) * 1024 * 1024;
    let restart_bytes = std::env::var("HIVE_MEM_RESTART_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(16384 * 1024 * 1024);
    let cooldown_ms = env_u64("HIVE_MEM_DUMP_COOLDOWN_SECS", 120) * 1000;
    let keep = env_u64("HIVE_MEM_DUMP_KEEP", 8) as usize;
    let prof_enabled = std::env::var("HIVE_MEM_PROF").ok().as_deref() == Some("1");

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let Some(rss) = rss_bytes() else {
                return;
            };

            if rss >= arm_bytes {
                let snap = snapshot(Some(&hive));
                tracing::warn!(
                    rss_mb = rss / 1024 / 1024,
                    arm_mb = arm_bytes / 1024 / 1024,
                    snapshot = %snap,
                    "memory above watchdog arm threshold"
                );

                if prof_enabled {
                    arm_profiling();
                } else if !PROF_SKIP_WARNED.swap(true, Ordering::SeqCst) {
                    tracing::warn!(
                        "memory watchdog did NOT arm heap profiling (HIVE_MEM_PROF != 1) — \
                         dumps below will carry no allocation-site attribution. Set \
                         HIVE_MEM_PROF=1 to opt in: jemalloc's prof_backtrace_impl walks \
                         frame pointers and SIGSEGVs a release build that has none \
                         (fc-sanjose, 2026-10-10)"
                    );
                }
            }

            if rss >= dump_bytes {
                let now = hive_core::now_ms();
                let last = LAST_DUMP_MS.load(Ordering::Relaxed);
                if now.saturating_sub(last) >= cooldown_ms {
                    LAST_DUMP_MS.store(now, Ordering::Relaxed);
                    dump_profile(rss, keep);
                }
            }

            if restart_bytes > 0 && rss >= restart_bytes {
                tracing::error!(
                    rss_mb = rss / 1024 / 1024,
                    restart_mb = restart_bytes / 1024 / 1024,
                    "memory watchdog: RSS crossed the restart threshold — dumping a final \
                     profile and requesting a controlled systemd restart rather than waiting \
                     for the kernel OOM killer"
                );
                dump_profile(rss, keep);
                restart.request(crate::RestartReason::MemoryPressure);
                return;
            }
        }
    });
}

/// Turn jemalloc sampling on, once. A no-op if it is already active (an
/// operator may have enabled it via `GET /v1/debug/heap`).
#[cfg(target_os = "linux")]
fn arm_profiling() {
    if ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let already =
        unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0") }.unwrap_or(false);
    if already {
        return;
    }
    match unsafe { tikv_jemalloc_ctl::raw::write(b"prof.active\0", true) } {
        Ok(()) => tracing::warn!(
            "memory watchdog ARMED heap profiling (prof.active=true) — the next dump will \
             attribute the growth to allocation sites"
        ),
        Err(e) => tracing::warn!(error = %e, "memory watchdog could not enable prof.active"),
    }
}

#[cfg(not(target_os = "linux"))]
fn arm_profiling() {}

#[cfg(target_os = "linux")]
fn dump_profile(rss: u64, keep: usize) {
    let dir = dump_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(), "heap dump dir");
        return;
    }
    let path = dir.join(format!("heap-{}.prof", hive_core::now_ms()));
    let mut c_path = path.to_string_lossy().into_owned().into_bytes();
    c_path.push(0);
    match unsafe {
        tikv_jemalloc_ctl::raw::write(b"prof.dump\0", c_path.as_ptr() as *const std::ffi::c_char)
    } {
        Ok(()) => {
            DUMPS.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                rss_mb = rss / 1024 / 1024,
                path = %path.display(),
                "memory watchdog wrote a heap profile — analyze with \
                 `jeprof --show_bytes <binary> <file>`; jemalloc's own frames \
                 (prof_backtrace_impl/prof_tctx_create) sit at the top of every \
                 stack and are NOT the culprit"
            );
        }
        Err(e) => tracing::warn!(error = %e, "prof.dump failed (was profiling ever active?)"),
    }
    prune_dumps(keep);
}

#[cfg(not(target_os = "linux"))]
fn dump_profile(_rss: u64, _keep: usize) {}
