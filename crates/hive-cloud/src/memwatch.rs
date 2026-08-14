//! Memory observability + an episodic-burst catcher.
//!
//! Why this exists, stated as the gap it closes. The fleet's OOM kills are
//! BURSTS, not steady growth: fc-hongkong idled around 1 GB RSS and was killed
//! three times at 98,214,816 / 98,215,476 / 98,189,292 kB anon-rss — within
//! 0.03% of each other, i.e. a process consuming everything the host has, in
//! minutes, roughly hourly. Two hours of manual `jeprof` sampling never caught
//! one, so the profile that was collected only ever described the idle state.
//! Two things were missing and are provided here:
//!
//! 1. **The numbers that distinguish a leak from fragmentation.** Live sampled
//!    heap was ~80 MB against ~1.09 GB RSS (~7%) — which says most resident
//!    memory was NOT live malloc'd objects. Confirming that needs jemalloc's
//!    own `stats.allocated/active/resident/retained/mapped`, and nothing on the
//!    node could read them (`stats_print:true` writes at exit, and the process
//!    never exits gracefully enough to run the atexit hook).
//!    [`snapshot`] reads them live, via `GET /v1/debug/memory`.
//!
//! 2. **A profile taken DURING the burst.** A heap profile is only useful if
//!    sampling was already on when memory ran away, and sampling is off at boot
//!    on purpose. The watchdog ARMS sampling when RSS crosses a low threshold
//!    and DUMPS when it crosses a high one, so the next burst leaves a profile
//!    on disk instead of only a kernel kill line.
//!
//! Both halves are bounded and honest: dumps are rate-limited, capped in count,
//! and written under `$HIVE_DATA` (never `/tmp`, which a reboot wipes — the
//! `git::deploy_root` lesson); arming is logged at WARN naming the threshold
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

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// Read one `Key:  <n> kB` field out of `/proc/self/status`, in bytes.
/// `VmRSS` rather than `/proc/self/statm` deliberately: statm reports PAGES, and
/// converting needs the page size, which is 4 KiB on the fleet's x86_64 hosts
/// but 16/64 KiB on aarch64 — a silent 16x error in exactly the number the
/// watchdog's thresholds are compared against. The kernel already did the
/// conversion here.
fn proc_status_kb(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            let n: u64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .ok()?;
            return Some(n.saturating_mul(1024));
        }
    }
    None
}

/// This process's resident set, in bytes. `None` off Linux or if `/proc` is
/// unreadable — never a guess, because a fabricated RSS would arm the watchdog
/// wrongly (or, worse, never arm it).
pub fn rss_bytes() -> Option<u64> {
    proc_status_kb("VmRSS:")
}

/// Live threads in this process (`/proc/self/status: Threads`). The report's
/// fragmentation hypothesis hangs on this number — jemalloc's per-arena dirty
/// pages scale with thread count, and this process was running ~195 threads.
fn thread_count() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Threads:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// jemalloc's own accounting. `epoch` MUST be advanced first: every `stats.*`
/// mallctl reads a cached snapshot that only refreshes on an epoch write, so
/// reading without it returns the same numbers forever — which looks exactly
/// like "memory is stable" and is the most misleading possible failure.
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
        // Bytes in live allocations. The "is it a leak?" number.
        "allocated": read(b"stats.allocated\0"),
        // Bytes in pages jemalloc is using for allocations (incl. per-run slop).
        "active": read(b"stats.active\0"),
        // Physically resident bytes jemalloc believes it holds. `resident`
        // minus `allocated` is the fragmentation/dirty-page gap that the
        // 80 MB-live-vs-1.09 GB-RSS reading pointed at.
        "resident": read(b"stats.resident\0"),
        // Virtual, returned to the OS but not unmapped — NOT resident.
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
        // Bounds this node enforces, as live gauges — a bound nobody can read
        // is indistinguishable from no bound at all.
        "bounds": {
            // hive_core::logcap — subprocess output capture.
            "log_lines_captured": log_lines,
            "log_lines_truncated": log_truncated,
            "log_bytes_dropped": log_dropped_bytes,
            "log_line_cap_bytes": hive_core::MAX_LOG_LINE_BYTES,
            // hive_controlplane — retained job records / replay buffers.
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

/// Keep only the newest `keep` dumps. Bounded like every other reclaim path
/// here: it only ever deletes files it created (`heap-*.prof` in its own
/// directory), never an empty-keep-set wipe of somebody else's data.
fn prune_dumps(keep: usize) {
    let dir = dump_dir();
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = match std::fs::read_dir(&dir)
    {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("heap-")
            })
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
    files.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// Watchdog loop. Runs on EVERY node (like the container lock sweep — the
/// resource is per-host), not leader-only.
///
/// Thresholds, all env-tunable, all in MiB:
/// * `HIVE_MEM_ARM_MB`   (default 3072) — turn jemalloc sampling on. Chosen
///   well above the ~1–1.8 GB idle band so a normal node never pays for it,
///   and well below the ~93 GiB kill point so there is time to sample.
/// * `HIVE_MEM_DUMP_MB`  (default 8192) — write a profile.
/// * `HIVE_MEM_RESTART_MB` (default 16384) — self-restart. A confirmed real
///   leak (not fragmentation — see the module doc's "80 MB live vs 1.09 GB
///   RSS" contrast, which does NOT hold here: a live profile taken during a
///   real burst sampled ~9.9 GiB of live-allocated bytes against ~11.3 GiB
///   RSS, i.e. genuinely leaked memory, not idle pages) left unattended
///   crosses into real OOM territory and drags the node's mesh connectivity
///   down with it — measured live: a node at this RSS band stops answering
///   gossip promptly enough to be reachable, and peers correctly mark it
///   isolated. `Restart=always` in the systemd unit means a clean exit here
///   is a full recovery, not a crash — this mirrors the exact manual
///   `systemctl restart hive-node` recovery already proven live, just
///   automatic instead of requiring an operator to notice. Set above
///   `HIVE_MEM_DUMP_MB` so a profile is always captured before the exit that
///   would otherwise erase the evidence.
/// * `HIVE_MEM_WATCH_SECS` (default 15) — tick interval. Must be short relative
///   to the burst (minutes), or the watchdog samples only the corpse.
/// * `HIVE_MEM_DUMP_COOLDOWN_SECS` (default 120), `HIVE_MEM_DUMP_KEEP` (8).
///
/// `HIVE_MEM_WATCH=0` disables it entirely. `HIVE_MEM_RESTART_MB=0` disables
/// only the restart tier — arm/dump still run.
pub fn spawn(hive: std::sync::Arc<hive_controlplane::Hive>) {
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

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let Some(rss) = rss_bytes() else {
                // No /proc — not Linux. Nothing to watch; stop the loop rather
                // than spin forever reading a file that will never exist.
                return;
            };
            if rss < arm_bytes {
                continue;
            }

            // Above the arm threshold: emit the full reading every tick. This
            // is the record that survives the kill — the kernel's OOM line
            // reports one number and no attribution.
            let snap = snapshot(Some(&hive));
            tracing::warn!(
                rss_mb = rss / 1024 / 1024,
                arm_mb = arm_bytes / 1024 / 1024,
                snapshot = %snap,
                "memory above watchdog arm threshold"
            );

            arm_profiling();

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
                     profile, flushing state, and exiting for a clean systemd restart \
                     (Restart=always) rather than waiting for the kernel OOM killer"
                );
                // One last profile, ignoring the normal cooldown — this exit is about
                // to make the process (and its heap) unavailable to inspect.
                dump_profile(rss, keep);
                // The background persister writes on its own cadence; without an
                // explicit flush here, whatever changed since its last tick is lost
                // on exit — the exact hazard `persist::flush_blocking`'s SIGTERM
                // call site already exists to close, reused here for the same reason.
                crate::persist::flush_blocking();
                std::process::exit(17);
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
    let already = unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0") }.unwrap_or(false);
    if already {
        return;
    }
    match unsafe { tikv_jemalloc_ctl::raw::write(b"prof.active\0", true) } {
        Ok(()) => tracing::warn!(
            "memory watchdog ARMED heap profiling (prof.active=true) — the next dump will \
             attribute the growth to allocation sites"
        ),
        // Not fatal: a binary built without profiling still gets the gauges
        // above, which is strictly more than the fleet had before.
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
