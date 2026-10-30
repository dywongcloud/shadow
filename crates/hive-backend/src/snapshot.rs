//! Copy-on-write snapshots of per-deployment data disks via XFS `reflink=1` (the fleet has no ZFS):
//! instant, consuming only what later diverges. Capability is per NODE and probed functionally
//! ([`snapshot_support`]); ext4 nodes degrade to a full copy and say so (`Method::Copy`). A
//! snapshot of a live image is CRASH-consistent only (callers wanting application consistency
//! quiesce the writer first); [`SnapshotOutcome::consistency`] records which guarantee was
//! obtained.

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

/// Whether `dir`'s filesystem supports reflink, tested FUNCTIONALLY (one tiny create + clone +
/// unlink) rather than by reading the filesystem type.
pub fn snapshot_support(dir: &Path) -> bool {
    let probe = dir.join(format!(".reflink-probe-{}", std::process::id()));
    let clone = dir.join(format!(".reflink-probe-{}-c", std::process::id()));
    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(&clone);
    if std::fs::write(&probe, b"x").is_err() {
        return false;
    }
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

/// Reject anything that is not a bare, safe filename component (`[A-Za-z0-9._-]`, no `..`, no
/// leading `.`): snapshot ids reach here from tenant-facing surfaces and are joined into a path.
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != ".."
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        && !s.contains("..")
}

/// Snapshot `image` into `dest_dir/{snap_id}.snap.ext4`; `quiesced` only labels the outcome.
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
    anyhow::ensure!(
        image.is_file(),
        "source image does not exist: {}",
        image.display()
    );
    std::fs::create_dir_all(dest_dir)?;
    let dest = dest_dir.join(format!("{snap_id}.snap.ext4"));
    anyhow::ensure!(
        !dest.exists(),
        "snapshot already exists: {}",
        dest.display()
    );

    let t0 = std::time::Instant::now();
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
        consistency: if quiesced {
            Consistency::Application
        } else {
            Consistency::Crash
        },
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

/// Count existing snapshots under `dest_dir` (for the storage broker's quota check).
pub fn count_snapshots(dest_dir: &Path) -> usize {
    list_snapshots(dest_dir).len()
}

/// List existing snapshot ids under `dest_dir` (order unspecified); a missing directory reads as
/// "no snapshots yet".
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
