//! Tenant cells that outlive the process that launched them (`KillMode=process`). Ownership is TWO
//! labels, `hive.owner` (a stable node identity) + `hive.boot`: a cell is current (this boot),
//! stale (this owner, another boot: reaped), foreign (another owner: never touched, counted) or
//! legacy (unlabelled: reaped only as the host's sole supervised instance or under
//! `HIVE_CELL_ORPHAN_REAP_LEGACY=1`). [`reap_orphaned_cells`] runs once at boot,
//! `guard_volume_single_writer` before every launch that mounts a named volume; only
//! `hive-cell-*` podman containers are ever touched (Apple `container` cells are neither labelled
//! nor reaped).

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

/// Container-name prefix of every serverless cell (`hive-{cell id}` with cell
/// ids `cell-xxxxxxxx`).
pub const CELL_PREFIX: &str = "hive-cell-";
pub const LABEL_CELL: &str = "hive.cell";
pub const LABEL_BOOT: &str = "hive.boot";
pub const LABEL_OWNER: &str = "hive.owner";
pub const LABEL_NODE: &str = "hive.node";
/// Named per-project tenant volumes (`git.rs::container_volume_cfg`).
const VOLUME_PREFIX: &str = "hive-vol-";
/// Containers per podman invocation.
const BATCH: usize = 50;
/// `podman stop` grace for a stateless, sole-writer or surviving-newest cell.
const STOP_GRACE_SECS: &str = "10";
/// Bound on one read-only podman call (`ps`, `inspect`) of the boot reap.
const REAP_LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound on the launch guard's listing.
const GUARD_LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the launch guard waits for its detached stop/remove task.
const GUARD_STOP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the launch guard re-lists for a stale writer to disappear (a cell
/// another stopper is gracefully stopping stays `stopping` for up to 10 s).
const GUARD_SETTLE: Duration = Duration::from_secs(20);

static REAP_RAN: AtomicBool = AtomicBool::new(false);
/// The confirmed reap left nothing unattributable (no kept legacy cell), so the per-volume guard may
/// stand down; separate from [`REAP_RAN`], which is also false on macOS.
static PODMAN_CLEAN: AtomicBool = AtomicBool::new(false);

struct Owner {
    node: String,
    id: String,
}

static OWNER: OnceLock<Owner> = OnceLock::new();

/// This process's boot identity (128 bits from the OS-seeded hasher keys, pid and start time),
/// stamped on every cell as `hive.boot`.
pub fn boot_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let half = |salt: u64| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(nanos);
            h.write_u32(std::process::id());
            h.write_u64(salt);
            h.finish()
        };
        format!("{:016x}{:016x}", half(1), half(2))
    })
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Set this node's cell-ownership identity once at boot: a stable hash of `node` and the canonical
/// `data_dir`, equal across this node's restarts and unique per hive-cloud process on the host.
pub fn set_cell_owner(node: &str, data_dir: &std::path::Path) {
    let dir = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let key = format!("{node}\0{}", dir.display());
    let _ = OWNER.set(Owner {
        node: node.to_string(),
        id: format!("{:016x}", fnv1a64(key.as_bytes())),
    });
}

/// Whether a boot reap has been CONFIRMED complete (a re-list showed no stale or permitted-legacy
/// cell left) — the precondition for any automatic restart; false when opted out
/// (`HIVE_CELL_ORPHAN_REAP=0`), while running, on a listing/parse failure, and always on macOS.
pub fn orphan_reap_ran() -> bool {
    REAP_RAN.load(Ordering::Relaxed)
}

/// The `podman run` label flags for one cell.
pub(crate) fn cell_label_args(cell_id: &str) -> Vec<String> {
    let mut labels = vec![
        format!("{LABEL_CELL}={cell_id}"),
        format!("{LABEL_BOOT}={}", boot_id()),
    ];
    if let Some(owner) = OWNER.get() {
        labels.push(format!("{LABEL_OWNER}={}", owner.id));
        labels.push(format!("{LABEL_NODE}={}", owner.node));
    }
    labels
        .into_iter()
        .flat_map(|label| ["--label".to_string(), label])
        .collect()
}

/// Whether label-less (pre-ownership) cells may be removed by this process.
fn legacy_reap_allowed() -> bool {
    match std::env::var("HIVE_CELL_ORPHAN_REAP_LEGACY")
        .as_deref()
        .map(str::trim)
    {
        Ok("1") => true,
        Ok("0") => false,
        _ => std::env::var_os("INVOCATION_ID").is_some_and(|v| !v.is_empty()),
    }
}

/// Outcome of one boot reap.
#[derive(Clone, Debug, Default)]
pub struct ReapReport {
    /// `hive-cell-*` containers listed.
    pub listed: usize,
    /// Of those, the ones this reap removes (stale, plus legacy when allowed).
    pub stale: usize,
    /// Stale cells confirmed removed.
    pub reaped: usize,
    /// Live stale cells SIGKILLed (duplicate writers of a shared volume).
    pub killed: usize,
    /// Live stale cells stopped gracefully.
    pub stopped: usize,
    /// Stale cells removed per named tenant volume.
    pub per_volume: BTreeMap<String, usize>,
    /// Cells of ANOTHER hive-cloud process on this host (never touched).
    pub foreign: usize,
    /// Label-less cells kept because legacy removal is not allowed here.
    pub legacy_kept: usize,
    /// Why nothing was attempted, when nothing was.
    pub skipped: Option<String>,
    /// Whether the confirming re-list showed nothing stale left
    /// (`orphan_reap_ran`).
    pub confirmed: bool,
    /// Stale cells still present after the reap (first 20).
    pub survivors: Vec<String>,
    /// What this reap does not cover even when confirmed (macOS: Apple
    /// `container` cells).
    pub gap: Option<String>,
}

/// One `hive-cell-*` container as far as the reaper cares.
struct Cell {
    id: String,
    name: String,
    boot: Option<String>,
    owner: Option<String>,
    state: String,
    created: i64,
}

impl Cell {
    /// Anything that may still have a process writing: running, paused,
    /// stopping, removing, or a state this code does not know.
    fn live(&self) -> bool {
        !matches!(
            self.state.to_ascii_lowercase().as_str(),
            "exited" | "created" | "configured" | "initialized" | "stopped"
        )
    }
}

/// Whose a listed cell is — see the module doc.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Claim {
    Current,
    Stale,
    Foreign,
    Legacy,
}

fn claim(cell: &Cell) -> Claim {
    if cell.boot.as_deref() == Some(boot_id()) {
        return Claim::Current;
    }
    match (cell.owner.as_deref(), OWNER.get()) {
        (Some(theirs), Some(mine)) if theirs == mine.id => Claim::Stale,
        (Some(_), _) => Claim::Foreign,
        (None, _) => Claim::Legacy,
    }
}

/// Whether this process removes `cell` (given the legacy decision).
fn removable(cell: &Cell, legacy: bool) -> bool {
    match claim(cell) {
        Claim::Stale => true,
        Claim::Legacy => legacy,
        Claim::Current | Claim::Foreign => false,
    }
}

fn first_name(entry: &Value) -> Option<String> {
    match entry.get("Names") {
        Some(Value::Array(names)) => names.first().and_then(Value::as_str).map(str::to_string),
        Some(Value::String(name)) => Some(name.clone()),
        _ => None,
    }
    .map(|n| n.trim_start_matches('/').to_string())
}

fn label(entry: &Value, key: &str) -> Option<String> {
    entry
        .get("Labels")
        .and_then(|labels| labels.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Parse `podman ps --format json`; an unrecognized shape is an ERROR, never an empty list (which
/// would switch the reap off while reporting success).
fn parse_cells(stdout: &[u8]) -> Result<Vec<Cell>, String> {
    let value: Value = serde_json::from_slice(stdout)
        .map_err(|e| format!("`ps --format json` output is not JSON: {e}"))?;
    let Value::Array(entries) = value else {
        return Err("`ps --format json` output is not a JSON array".into());
    };
    let mut cells = Vec::new();
    for entry in &entries {
        let Some(name) = first_name(entry) else {
            return Err(format!(
                "a `ps --format json` entry carries no Names: {}",
                entry.to_string().chars().take(200).collect::<String>()
            ));
        };
        if !name.starts_with(CELL_PREFIX) {
            continue;
        }
        let Some(state) = entry.get("State").and_then(Value::as_str) else {
            return Err(format!(
                "`ps --format json` entry {name} carries no string State"
            ));
        };
        cells.push(Cell {
            id: entry
                .get("Id")
                .or_else(|| entry.get("ID"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            boot: label(entry, LABEL_BOOT),
            owner: label(entry, LABEL_OWNER),
            state: state.to_string(),
            created: entry.get("Created").and_then(Value::as_i64).unwrap_or(0),
            name,
        });
    }
    Ok(cells)
}

/// One podman call. `kill_on_drop` only for READ-ONLY verbs: a mutating one
/// (`kill`, `stop`, `rm`) cancelled halfway leaves a half-removed container.
async fn podman(
    bin: &str,
    path_env: &str,
    args: &[String],
    kill_on_drop: bool,
) -> std::io::Result<std::process::Output> {
    Command::new(bin)
        .args(args)
        .env("PATH", path_env)
        .kill_on_drop(kill_on_drop)
        .output()
        .await
}

/// A read-only podman call under `limit`; a timeout or non-zero exit is an
/// error naming the call.
async fn podman_read(
    bin: &str,
    path_env: &str,
    args: &[String],
    limit: Duration,
) -> Result<Vec<u8>, String> {
    let call = args.join(" ");
    match tokio::time::timeout(limit, podman(bin, path_env, args, true)).await {
        Err(_) => Err(format!(
            "`{bin} {call}` timed out after {}s",
            limit.as_secs()
        )),
        Ok(Err(e)) => Err(format!("`{bin} {call}` failed to run: {e}")),
        Ok(Ok(o)) if !o.status.success() => Err(format!(
            "`{bin} {call}` failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Ok(Ok(o)) => Ok(o.stdout),
    }
}

fn args(head: &[&str], names: &[String]) -> Vec<String> {
    head.iter()
        .map(|s| s.to_string())
        .chain(names.iter().cloned())
        .collect()
}

/// List every `hive-cell-*` container, cross-checked against `ps -a -q --no-trunc` so a JSON shape
/// that silently drops entries is caught; the id listing runs FIRST.
async fn list_cells(
    bin: &str,
    path_env: &str,
    filter: Option<&str>,
    limit: Duration,
) -> Result<Vec<Cell>, String> {
    let mut ids_call = vec![
        "ps",
        "-a",
        "-q",
        "--no-trunc",
        "--filter",
        "name=^hive-cell-",
    ];
    let mut json_call = vec!["ps", "-a", "--format", "json"];
    if let Some(f) = filter {
        ids_call.extend(["--filter", f]);
        json_call.extend(["--filter", f]);
    }
    let ids_out = podman_read(bin, path_env, &args(&ids_call, &[]), limit).await?;
    let json_out = podman_read(bin, path_env, &args(&json_call, &[]), limit).await?;
    let cells = parse_cells(&json_out)?;
    let ids: Vec<String> = String::from_utf8_lossy(&ids_out)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let parsed: HashSet<&str> = cells.iter().map(|c| c.id.as_str()).collect();
    let missing = ids
        .iter()
        .filter(|id| {
            !parsed.contains(id.as_str())
                && !parsed.iter().any(|p| !p.is_empty() && id.starts_with(p))
        })
        .count();
    if missing > 0 {
        return Err(format!(
            "`ps -q` names {} hive-cell container(s) the JSON listing did not parse ({missing} missing) \
             -- refusing to act on an incomplete view",
            ids.len()
        ));
    }
    Ok(cells)
}

/// Named `hive-vol-*` volumes each container mounts (`podman inspect`); a batch that cannot be
/// inspected only ever selects the graceful stop, never a SIGKILL.
async fn volumes_of(bin: &str, path_env: &str, names: &[String]) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    for chunk in names.chunks(BATCH) {
        let Ok(stdout) = podman_read(
            bin,
            path_env,
            &args(&["inspect", "--type", "container"], chunk),
            REAP_LIST_TIMEOUT,
        )
        .await
        else {
            continue;
        };
        let Ok(Value::Array(entries)) = serde_json::from_slice::<Value>(&stdout) else {
            continue;
        };
        for entry in &entries {
            let Some(name) = entry
                .get("Name")
                .and_then(Value::as_str)
                .map(|n| n.trim_start_matches('/').to_string())
            else {
                continue;
            };
            let vols: Vec<String> = entry
                .get("Mounts")
                .and_then(Value::as_array)
                .map(|mounts| {
                    mounts
                        .iter()
                        .filter(|m| m.get("Type").and_then(Value::as_str) == Some("volume"))
                        .filter_map(|m| m.get("Name").and_then(Value::as_str))
                        .filter(|v| v.starts_with(VOLUME_PREFIX))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            out.insert(name, vols);
        }
    }
    out
}

/// How each live stale cell is stopped: `kill` (SIGKILL, first) and `stop`
/// (graceful, after every kill) — see the module doc.
#[derive(Default)]
struct StopPlan {
    kill: Vec<String>,
    stop: Vec<String>,
}

/// `live` is every live cell in view (any claim); `stale` names the live ones
/// to stop.
fn plan_stops(
    live: &[&Cell],
    stale: &HashSet<String>,
    volumes: &HashMap<String, Vec<String>>,
) -> StopPlan {
    let mut writers: BTreeMap<&str, Vec<&Cell>> = BTreeMap::new();
    for cell in live {
        for v in volumes.get(&cell.name).into_iter().flatten() {
            writers.entry(v.as_str()).or_default().push(cell);
        }
    }
    let mut kill = BTreeSet::new();
    for cells in writers.values().filter(|cells| cells.len() >= 2) {
        let protected = cells.iter().any(|c| !stale.contains(&c.name));
        let survivor = (!protected)
            .then(|| {
                cells
                    .iter()
                    .max_by(|a, b| (a.created, &a.name).cmp(&(b.created, &b.name)))
            })
            .flatten()
            .map(|c| c.name.as_str());
        for cell in cells.iter().filter(|c| stale.contains(&c.name)) {
            if Some(cell.name.as_str()) != survivor {
                kill.insert(cell.name.clone());
            }
        }
    }
    let mut stop: Vec<String> = stale
        .iter()
        .filter(|n| !kill.contains(*n))
        .cloned()
        .collect();
    stop.sort();
    StopPlan {
        kill: kill.into_iter().collect(),
        stop,
    }
}

async fn kill_all(bin: &str, path_env: &str, names: &[String]) {
    for chunk in names.chunks(BATCH) {
        let _ = podman(
            bin,
            path_env,
            &args(&["kill", "--signal", "KILL"], chunk),
            false,
        )
        .await;
    }
}

async fn stop_all(bin: &str, path_env: &str, names: &[String]) {
    for chunk in names.chunks(BATCH) {
        let _ = podman(
            bin,
            path_env,
            &args(&["stop", "-t", STOP_GRACE_SECS], chunk),
            false,
        )
        .await;
    }
}

/// `rm -f -v` every name; returns the names confirmed gone.
async fn remove_all(bin: &str, path_env: &str, names: &[String]) -> BTreeSet<String> {
    let mut removed = BTreeSet::new();
    for chunk in names.chunks(BATCH) {
        let ok = podman(bin, path_env, &args(&["rm", "-f", "-v"], chunk), false)
            .await
            .is_ok_and(|o| o.status.success());
        if ok {
            removed.extend(chunk.iter().cloned());
            continue;
        }
        for name in chunk {
            let gone = podman(
                bin,
                path_env,
                &args(&["rm", "-f", "-v"], std::slice::from_ref(name)),
                false,
            )
            .await
            .is_ok_and(|o| {
                o.status.success()
                    || String::from_utf8_lossy(&o.stderr)
                        .to_ascii_lowercase()
                        .contains("no such container")
            });
            if gone {
                removed.insert(name.clone());
            }
        }
    }
    removed
}

fn describe(cells: &[&Cell]) -> Vec<String> {
    cells
        .iter()
        .take(20)
        .map(|c| format!("{} ({})", c.name, c.state))
        .collect()
}

/// Boot reap — see the module doc. `urgent_done` fires once the listing is classified and every
/// duplicate writer of a shared volume is SIGKILLed; the graceful stops, removals and confirming
/// re-list follow. A listing podman cannot produce leaves [`orphan_reap_ran`] false.
pub async fn reap_orphaned_cells(
    bin: &str,
    path_env: &str,
    urgent_done: tokio::sync::oneshot::Sender<()>,
) -> ReapReport {
    let mut report = ReapReport::default();
    let apple_host = crate::container_cli::is_apple_default();
    if apple_host {
        report.gap =
            Some("Apple `container` cells on this macOS node are not labelled or reaped".into());
    }
    let cells = match list_cells(bin, path_env, None, REAP_LIST_TIMEOUT).await {
        Ok(cells) => cells,
        Err(_) if which_missing(bin, path_env).await => {
            report.skipped = Some(format!("{bin} is not installed"));
            REAP_RAN.store(!apple_host, Ordering::Relaxed);
            PODMAN_CLEAN.store(true, Ordering::Relaxed);
            return report;
        }
        Err(e) => {
            report.skipped = Some(e);
            return report;
        }
    };
    let legacy = legacy_reap_allowed();
    report.listed = cells.len();
    report.foreign = cells.iter().filter(|c| claim(c) == Claim::Foreign).count();
    report.legacy_kept = if legacy {
        0
    } else {
        cells.iter().filter(|c| claim(c) == Claim::Legacy).count()
    };
    let stale: Vec<&Cell> = cells.iter().filter(|c| removable(c, legacy)).collect();
    report.stale = stale.len();
    if !stale.is_empty() {
        let live: Vec<&Cell> = cells.iter().filter(|c| c.live()).collect();
        let inspect: Vec<String> = cells
            .iter()
            .filter(|c| c.live() || removable(c, legacy))
            .map(|c| c.name.clone())
            .collect();
        let volumes = volumes_of(bin, path_env, &inspect).await;
        let stale_live: HashSet<String> = stale
            .iter()
            .filter(|c| c.live())
            .map(|c| c.name.clone())
            .collect();
        let plan = plan_stops(&live, &stale_live, &volumes);
        kill_all(bin, path_env, &plan.kill).await;
        let _ = urgent_done.send(());
        stop_all(bin, path_env, &plan.stop).await;
        report.killed = plan.kill.len();
        report.stopped = plan.stop.len();
        let names: Vec<String> = stale.iter().map(|c| c.name.clone()).collect();
        let removed = remove_all(bin, path_env, &names).await;
        report.reaped = removed.len();
        for name in &removed {
            for v in volumes.get(name).into_iter().flatten() {
                *report.per_volume.entry(v.clone()).or_default() += 1;
            }
        }
    } else {
        let _ = urgent_done.send(());
    }
    match list_cells(bin, path_env, None, REAP_LIST_TIMEOUT).await {
        Ok(after) => {
            let left: Vec<&Cell> = after.iter().filter(|c| removable(c, legacy)).collect();
            report.survivors = describe(&left);
            report.confirmed = left.is_empty();
        }
        Err(e) => report.survivors = vec![format!("confirming re-list failed: {e}")],
    }
    REAP_RAN.store(report.confirmed && !apple_host, Ordering::Relaxed);
    PODMAN_CLEAN.store(
        report.confirmed && report.legacy_kept == 0,
        Ordering::Relaxed,
    );
    report
}

/// Whether `bin` itself is absent (the listing failed because there is no
/// podman at all, not because podman misbehaved).
async fn which_missing(bin: &str, path_env: &str) -> bool {
    matches!(
        podman(bin, path_env, &args(&["--version"], &[]), true).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// Launch-time single-writer guard for one named volume (`own` is never counted): fails with a
/// `NODE_BACKEND_UNAVAILABLE` node fault only when a live stale writer provably survives being
/// stopped and removed; a failed listing is WARNed and the launch proceeds.
pub(crate) async fn guard_volume_single_writer(
    bin: &'static str,
    path_env: &str,
    volume: &str,
    own: &str,
) -> anyhow::Result<()> {
    if !volume.starts_with(VOLUME_PREFIX) || PODMAN_CLEAN.load(Ordering::Relaxed) {
        return Ok(());
    }
    let filter = format!("volume={volume}");
    let legacy = legacy_reap_allowed();
    let cells = match list_cells(bin, path_env, Some(&filter), GUARD_LIST_TIMEOUT).await {
        Ok(cells) => cells,
        Err(e) => {
            tracing::warn!(
                volume,
                launching = own,
                error = %e,
                "cell volume guard: could not list this volume's writers -- launching WITHOUT \
                 the single-writer check"
            );
            return Ok(());
        }
    };
    let live: Vec<&Cell> = cells.iter().filter(|c| c.live() && c.name != own).collect();
    let foreign: Vec<&Cell> = live
        .iter()
        .copied()
        .filter(|c| claim(c) == Claim::Foreign || (claim(c) == Claim::Legacy && !legacy))
        .collect();
    if !foreign.is_empty() {
        tracing::warn!(
            volume,
            launching = own,
            others = ?describe(&foreign),
            "cell volume guard: another hive-cloud process's (or an unattributable legacy) cell \
             is writing this volume -- left alone; two processes serving one project on one host \
             share its volume"
        );
    }
    let stale: HashSet<String> = live
        .iter()
        .filter(|c| removable(c, legacy))
        .map(|c| c.name.clone())
        .collect();
    if stale.is_empty() {
        return Ok(());
    }
    let volumes: HashMap<String, Vec<String>> = live
        .iter()
        .map(|c| (c.name.clone(), vec![volume.to_string()]))
        .collect();
    let plan = plan_stops(&live, &stale, &volumes);
    let (killed, stopped) = (plan.kill.len(), plan.stop.len());
    let task = {
        let path_env = path_env.to_string();
        let remove: Vec<String> = stale.iter().cloned().collect();
        tokio::spawn(async move {
            kill_all(bin, &path_env, &plan.kill).await;
            stop_all(bin, &path_env, &plan.stop).await;
            remove_all(bin, &path_env, &remove).await
        })
    };
    let removed = match tokio::time::timeout(GUARD_STOP_TIMEOUT, task).await {
        Ok(Ok(removed)) => Some(removed.len()),
        _ => None,
    };
    let deadline = tokio::time::Instant::now() + GUARD_SETTLE;
    let left = loop {
        let left = match list_cells(bin, path_env, Some(&filter), GUARD_LIST_TIMEOUT).await {
            Ok(after) => after
                .iter()
                .filter(|c| c.live() && stale.contains(&c.name))
                .map(|c| format!("{} ({})", c.name, c.state))
                .collect::<Vec<_>>(),
            Err(e) => vec![format!("re-list failed: {e}")],
        };
        if left.is_empty() || tokio::time::Instant::now() >= deadline {
            break left;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    if !left.is_empty() {
        tracing::error!(
            volume,
            launching = own,
            stale = stale.len(),
            killed,
            stopped,
            still_live = ?left,
            "cell volume guard: a stale-generation writer is STILL live on this volume -- refusing \
             the launch rather than start a second writer"
        );
        anyhow::bail!(
            "{}: a stale hive-cell writer from a previous boot of this node is still live on \
             volume {volume} ({}) after stop + rm -f; refusing to start a second writer (operator \
             remedy: `podman rm -f -v` it by hand)",
            hive_core::fault::NODE_BACKEND_UNAVAILABLE,
            left.join(", ")
        );
    }
    tracing::warn!(
        volume,
        launching = own,
        stale = stale.len(),
        killed,
        stopped,
        removed = removed
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unconfirmed (stop/remove still running)".into()),
        "cell volume guard: stale-generation writer(s) of this volume gone before launch"
    );
    Ok(())
}
