//! Copy-on-write snapshots of per-deployment data disks.
//!
//! # Why reflink and not ZFS
//!
//! The obvious design for this is ZFS datasets + `zfs snapshot`. This fleet has
//! no ZFS: measured 2026-07-31, no `zfs`/`zpool` binary and no kernel module on
//! any of 14 nodes, and no spare block devices to build a pool from (e.g.
//! fc-frankfurt's single 1000 GiB disk is fully allocated to `/`). Installing it
//! would mean repartitioning every node.
//!
//! What the fleet *does* have is XFS with `reflink=1`, which provides the same
//! copy-on-write primitive at file granularity. Measured on real hardware
//! (fc-frankfurt, 3 GiB image containing 800 MiB of data):
//!
//! ```text
//!   reflink snapshot   ->      0 MiB consumed, instant
//!   plain copy         ->   1034 MiB consumed
//!   write 50 MiB into the snapshot afterwards -> 51 MiB consumed
//! ```
//!
//! So a snapshot is free to take and costs only what subsequently diverges —
//! the property that makes frequent snapshots viable at all.
//!
//! # Capability is per-NODE, and that is deliberate
//!
//! The fleet is split: 9 nodes are XFS+reflink (bkk, hk, sp, fr, gpusj1/2/3,
//! cvmsj1/2) and 5 are ext4 with no reflink at all (va, va2, va3, sj, sj2).
//! ext4 has neither reflink nor `FIDEDUPERANGE`, so there is no way to fake it
//! there. Rather than pretend, [`snapshot_support`] reports the real capability
//! and [`snapshot_image`] degrades to a full copy with `Method::Copy` — the
//! caller learns which it got. A snapshot that silently costs full size (and
//! full time) on some nodes and not others is worse than one that says so,
//! because the difference decides whether a scheduled snapshot is affordable.
//!
//! # Consistency is the caller's job, and it is the hard part
//!
//! A reflink of a live, mounted, actively-written image is CRASH-consistent, not
//! application-consistent: it captures whatever was on disk at that instant,
//! including a half-written transaction. That is fine for a filesystem with a
//! journal and wrong for a database that has not flushed. Callers wanting an
//! application-consistent snapshot must quiesce the writer first (checkpoint /
//! WAL flush / freeze), and [`SnapshotOutcome::consistency`] records which
//! guarantee the caller actually obtained so a restore path is never misled
//! about what it holds.

use std::path::{Path, PathBuf};

/// How the snapshot was actually taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Method {
    /// Copy-on-write clone: no space consumed at creation, diverging blocks
    /// billed as they are written.
    Reflink,
    /// Full byte copy: consumes the image's allocated size immediately. The
    /// honest fallback on a filesystem with no CoW support.
    Copy,
}

/// What the snapshot actually guarantees about the data inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Consistency {
    /// The writer was quiesced (checkpoint/flush/freeze) before the snapshot.
    Application,
    /// Taken against a live image. Equivalent to a power-cut: journal-replayable,
    /// but an application mid-transaction may need its own recovery.
    Crash,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SnapshotOutcome {
    pub path: PathBuf,
    pub method: Method,
    pub consistency: Consistency,
    /// Apparent size of the snapshot, bytes.
    pub bytes: u64,
    pub took_ms: u64,
}

/// Whether `dir`'s filesystem supports reflink, tested FUNCTIONALLY rather than
/// by reading the filesystem type.
///
/// `xfs_info` reporting `reflink=1` is necessary and not sufficient — the mount,
/// the kernel and the specific directory all have to cooperate, and the cheapest
/// honest answer is to attempt the operation once. Costs one tiny file create +
/// clone + unlink.
pub fn snapshot_support(dir: &Path) -> bool {
    let probe = dir.join(format!(".reflink-probe-{}", std::process::id()));
    let clone = dir.join(format!(".reflink-probe-{}-c", std::process::id()));
    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(&clone);
    if std::fs::write(&probe, b"x").is_err() {
        return false;
    }
    // stderr suppressed: a failure here is an EXPECTED outcome on any
    // non-CoW filesystem, and on macOS (BSD cp, no --reflink flag at all) it
    // prints a usage block. Probing must be silent or every snapshot on a dev
    // machine spams the log with a non-error.
    let ok = std::process::Command::new("cp")
        .arg("--reflink=always")
        .arg(&probe)
        .arg(&clone)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(&clone);
    ok
}

/// Reject anything that is not a bare, safe filename component.
///
/// Snapshot ids reach this module from tenant-facing surfaces, and the
/// destination path is built by joining. Without this, `..%2f..` or an absolute
/// id would place (or overwrite) a file anywhere the process can write — the
/// same class of bug as passing a tenant string into a mount argument, which is
/// exactly how a storage appliance ends up exposing the host. Allowing only
/// `[A-Za-z0-9._-]`, forbidding `..`, and refusing a leading `.` keeps the
/// result a single component under `dest_dir` by construction rather than by
/// the caller remembering to check.
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != ".."
        && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        && !s.contains("..")
}

/// Snapshot `image` into `dest_dir/{snap_id}.snap.ext4`.
///
/// `quiesced` states whether the caller already flushed/froze the writer; it
/// only labels the outcome — this function never assumes it.
pub fn snapshot_image(
    image: &Path,
    dest_dir: &Path,
    snap_id: &str,
    quiesced: bool,
) -> anyhow::Result<SnapshotOutcome> {
    anyhow::ensure!(
        safe_component(snap_id),
        "invalid snapshot id {snap_id:?}: must be a single [A-Za-z0-9._-] component, \
         no leading dot, no '..'"
    );
    anyhow::ensure!(image.is_file(), "source image does not exist: {}", image.display());
    std::fs::create_dir_all(dest_dir)?;
    let dest = dest_dir.join(format!("{snap_id}.snap.ext4"));
    anyhow::ensure!(!dest.exists(), "snapshot already exists: {}", dest.display());

    let t0 = std::time::Instant::now();
    // Try CoW first, fall back to a real copy. `--reflink=auto` would silently
    // do the same thing but would not tell us WHICH happened, and the caller
    // needs that: one is free, the other costs the image's full size.
    let reflinked = std::process::Command::new("cp")
        .arg("--reflink=always")
        .arg(image)
        .arg(&dest)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let method = if reflinked {
        Method::Reflink
    } else {
        std::fs::copy(image, &dest)
            .map_err(|e| anyhow::anyhow!("snapshot copy failed for {}: {e}", image.display()))?;
        Method::Copy
    };

    let bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    let outcome = SnapshotOutcome {
        path: dest,
        method,
        consistency: if quiesced { Consistency::Application } else { Consistency::Crash },
        bytes,
        took_ms: t0.elapsed().as_millis() as u64,
    };
    tracing::info!(
        image = %image.display(),
        snapshot = %outcome.path.display(),
        method = ?outcome.method,
        consistency = ?outcome.consistency,
        mib = bytes / (1024 * 1024),
        took_ms = outcome.took_ms,
        "snapshot taken"
    );
    Ok(outcome)
}

/// Count existing snapshots under `dest_dir`. The storage broker's quota
/// check needs this number BEFORE deciding whether a create may proceed —
/// exposed here rather than left for each caller to re-derive the
/// `*.snap.ext4` naming convention itself, the same reasoning
/// `list_snapshots` below follows.
pub fn count_snapshots(dest_dir: &Path) -> usize {
    list_snapshots(dest_dir).len()
}

/// List existing snapshot ids under `dest_dir` (order unspecified — sort if a
/// caller needs one). Missing/unreadable directory reads as "no snapshots
/// yet", not an error: a project that has never snapshotted has no directory
/// at all until its first `snapshot_image` call creates it.
pub fn list_snapshots(dest_dir: &Path) -> Vec<String> {
    std::fs::read_dir(dest_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".snap.ext4").map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Delete a snapshot. Same id validation as creation — a delete path that
/// accepts `../..` is a worse bug than a create path that does.
pub fn delete_snapshot(dest_dir: &Path, snap_id: &str) -> anyhow::Result<bool> {
    anyhow::ensure!(safe_component(snap_id), "invalid snapshot id {snap_id:?}");
    let p = dest_dir.join(format!("{snap_id}.snap.ext4"));
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
