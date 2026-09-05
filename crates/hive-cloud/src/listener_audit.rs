//! Host listener audit — every node, periodic, node-local.
//!
//! The Tencent security group opens TCP+UDP 20000-29999 to the whole internet
//! on every platform host (published container ports ride the raw proxy
//! there), so ANY wildcard-bound listener in that range is world-reachable the
//! moment it binds. On 2026-08-26 two `python3 -m http.server --bind 0.0.0.0`
//! binary hand-offs were left on fc-virginia:28126/:28127 and served a
//! directory listing to the internet for a week; the first notice was the
//! cloud provider's vulnerability ticket, because nothing on the node looked.
//!
//! This module looks. Each pass reads `/proc/net/tcp` + `/proc/net/tcp6`,
//! keeps LISTEN sockets bound to a wildcard address inside the audited range,
//! drops the ones this process owns (the raw proxy's own listeners — matched
//! by socket inode against `/proc/self/fd`, never by port list), and resolves
//! the rest to a pid/cmdline/cgroup. Everything left is FOREIGN: it is not
//! hive-cloud, it is inside the internet-open range, and it is bound to every
//! interface. The report is node-local (`GET /v1/host/listeners`, the
//! `/v1/dns/stats` precedent: through the dashboard's `/ops/*` proxy you read
//! the LEADER's report, not the page-serving node's); the fleet view is
//! `scripts/audit-public-listeners.sh`.
//!
//! Detection only, never remediation: the platform must not kill a process
//! it did not start (an operator's deliberate tool, a tenant hand-off in
//! progress). It WARNs every pass while the exposure exists and, on the
//! control-plane leader, opens ONE deduplicated Major incident per
//! (port, pid) so the ops dashboard shows it within a pass instead of a
//! provider ticket showing it a week later.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::Serialize;

use crate::state::CloudState;

/// The internet-open published-port range (the Tencent SG rule; see
/// AGENTS.md "Host firewall & public listeners"). Overridable for a fleet
/// whose SG shape differs: `HIVE_LISTENER_AUDIT_PORTS=lo-hi`.
pub const DEFAULT_RANGE: (u16, u16) = (20000, 29999);

const CMDLINE_CAP: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct ForeignListener {
    pub port: u16,
    /// `v4` or `v6` — which `/proc/net/tcp*` table the socket came from.
    pub family: &'static str,
    pub inode: u64,
    /// `None` when no `/proc/<pid>/fd` entry references the inode (the owner
    /// exited between the table read and the fd walk, or `/proc` is hidden).
    pub pid: Option<u32>,
    pub cmd: String,
    pub cgroup: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuditReport {
    pub node: String,
    pub checked_ms: u64,
    pub elapsed_ms: u64,
    /// `false` on a host without `/proc/net/tcp` (macOS dev nodes): nothing
    /// was audited, which is different from "audited and clean".
    pub supported: bool,
    pub range: (u16, u16),
    pub self_pid: u32,
    /// Wildcard LISTEN sockets in range owned by this process (the raw proxy).
    pub own_in_range: usize,
    pub foreign: Vec<ForeignListener>,
}

static LAST: OnceLock<RwLock<Option<AuditReport>>> = OnceLock::new();

fn last_slot() -> &'static RwLock<Option<AuditReport>> {
    LAST.get_or_init(|| RwLock::new(None))
}

/// The most recent pass on THIS node, if one has run.
pub fn last() -> Option<AuditReport> {
    last_slot().read().clone()
}

pub fn audited_range() -> (u16, u16) {
    let Ok(v) = std::env::var("HIVE_LISTENER_AUDIT_PORTS") else {
        return DEFAULT_RANGE;
    };
    let Some((lo, hi)) = v.split_once('-') else {
        return DEFAULT_RANGE;
    };
    match (lo.trim().parse::<u16>(), hi.trim().parse::<u16>()) {
        (Ok(lo), Ok(hi)) if lo <= hi => (lo, hi),
        _ => DEFAULT_RANGE,
    }
}

fn interval_secs() -> u64 {
    std::env::var("HIVE_LISTENER_AUDIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300)
}

/// A wildcard-bound LISTEN socket from one `/proc/net/tcp*` table.
struct WildcardListen {
    port: u16,
    inode: u64,
    family: &'static str,
}

/// Parse one `/proc/net/tcp` / `tcp6` table into its wildcard LISTEN rows.
/// Row shape: `sl local_address rem_address st ... uid timeout inode`;
/// `st == 0A` is LISTEN; the local address is `HEXIP:HEXPORT` with a 8-hex
/// (v4) or 32-hex (v6) ip. A wildcard bind is an all-zero ip in either width
/// (an IPv6 wildcard also accepts IPv4 unless `IPV6_V6ONLY`, so both count).
fn parse_table(text: &str, family: &'static str) -> Vec<WildcardListen> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let mut f = line.split_whitespace();
        let (_sl, local, _rem, st) = match (f.next(), f.next(), f.next(), f.next()) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => continue,
        };
        if st != "0A" {
            continue;
        }
        let Some((ip_hex, port_hex)) = local.rsplit_once(':') else {
            continue;
        };
        if !ip_hex.bytes().all(|b| b == b'0') {
            continue;
        }
        let Ok(port) = u16::from_str_radix(port_hex, 16) else {
            continue;
        };
        // Columns 5..=9 are tx/rx queue, tr/tm->when, retrnsmt, uid, timeout;
        // the inode is the 10th field (index 9 after the first four).
        let inode = f
            .nth(5)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        out.push(WildcardListen {
            port,
            inode,
            family,
        });
    }
    out
}

/// Socket inodes referenced by `/proc/<pid>/fd`.
fn socket_inodes_of(pid_dir: &str) -> HashSet<u64> {
    let mut set = HashSet::new();
    let Ok(entries) = fs::read_dir(format!("{pid_dir}/fd")) else {
        return set;
    };
    for e in entries.flatten() {
        let Ok(target) = fs::read_link(e.path()) else {
            continue;
        };
        let t = target.to_string_lossy();
        if let Some(rest) = t.strip_prefix("socket:[") {
            if let Some(num) = rest.strip_suffix(']') {
                if let Ok(n) = num.parse::<u64>() {
                    set.insert(n);
                }
            }
        }
    }
    set
}

/// Resolve foreign inodes to their owning pids by walking `/proc/<pid>/fd`.
/// Stops as soon as every wanted inode is found; a process that exits
/// mid-walk simply stays unresolved (`pid: None`).
fn owners_of(wanted: &HashSet<u64>) -> HashMap<u64, u32> {
    let mut found = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for e in entries.flatten() {
        if found.len() == wanted.len() {
            break;
        }
        let name = e.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        let dir = format!("/proc/{pid}");
        for ino in socket_inodes_of(&dir) {
            if wanted.contains(&ino) {
                found.entry(ino).or_insert(pid);
            }
        }
    }
    found
}

fn cmdline_of(pid: u32) -> String {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let mut s: String = String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if s.len() > CMDLINE_CAP {
        let mut cut = CMDLINE_CAP;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

fn cgroup_of(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()
        .and_then(|t| t.lines().next().map(|l| l.to_string()))
        .unwrap_or_default()
}

/// One synchronous pass. Blocking filesystem reads — call from
/// `spawn_blocking`, never on a runtime worker.
pub fn audit_once(node: &str) -> AuditReport {
    let started = Instant::now();
    let range = audited_range();
    let self_pid = std::process::id();
    let mut report = AuditReport {
        node: node.to_string(),
        checked_ms: hive_core::now_ms(),
        elapsed_ms: 0,
        supported: false,
        range,
        self_pid,
        own_in_range: 0,
        foreign: Vec::new(),
    };
    let Ok(tcp4) = fs::read_to_string("/proc/net/tcp") else {
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        return report;
    };
    report.supported = true;
    let tcp6 = fs::read_to_string("/proc/net/tcp6").unwrap_or_default();

    let mut rows = parse_table(&tcp4, "v4");
    rows.extend(parse_table(&tcp6, "v6"));
    rows.retain(|r| r.port >= range.0 && r.port <= range.1);

    let own = socket_inodes_of("/proc/self");
    let mut wanted: HashSet<u64> = HashSet::new();
    let mut candidates = Vec::new();
    for r in rows {
        if own.contains(&r.inode) {
            report.own_in_range += 1;
        } else {
            wanted.insert(r.inode);
            candidates.push(r);
        }
    }
    let owners = owners_of(&wanted);
    for c in candidates {
        let pid = owners.get(&c.inode).copied();
        report.foreign.push(ForeignListener {
            port: c.port,
            family: c.family,
            inode: c.inode,
            pid,
            cmd: pid.map(cmdline_of).unwrap_or_default(),
            cgroup: pid.map(cgroup_of).unwrap_or_default(),
        });
    }
    report.foreign.sort_by_key(|f| (f.port, f.family));
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report
}

fn incident_title(node: &str, f: &ForeignListener) -> String {
    match f.pid {
        Some(pid) => format!(
            "Foreign public listener on {node}: :{} pid {pid} ({})",
            f.port,
            f.cmd.split(' ').take(4).collect::<Vec<_>>().join(" ")
        ),
        None => format!("Foreign public listener on {node}: :{} (owner unresolved)", f.port),
    }
}

/// Open ONE Major incident per (node, port, pid) while it stays open — the
/// leader's incident store is what the ops dashboard and the follower sync
/// read, so this runs only where `leader` is true; a follower's finding is
/// its WARN line plus its own `/v1/host/listeners`.
fn raise_incidents(cloud: &Arc<CloudState>, report: &AuditReport) -> usize {
    use crate::incidents::{IncidentStatus, OpenReq, Severity};
    let open_titles: HashSet<String> = cloud
        .incidents
        .list()
        .into_iter()
        .filter(|i| i.status != IncidentStatus::Resolved)
        .map(|i| i.title)
        .collect();
    let mut opened = 0;
    for f in &report.foreign {
        let title = incident_title(&report.node, f);
        if open_titles.contains(&title) {
            continue;
        }
        cloud.incidents.open(OpenReq {
            title,
            severity: Severity::Major,
            affected: vec![report.node.clone()],
            message: format!(
                "A process that is not hive-cloud is listening on every interface at \
                 port {} (inside the internet-open published range {}-{}). pid={} cmd={:?} \
                 cgroup={:?}. Not started by the platform; nothing else guards this \
                 range. Kill it, or bind it to 127.0.0.1 behind an ssh tunnel \
                 (AGENTS.md \"Host firewall & public listeners\").",
                f.port,
                report.range.0,
                report.range.1,
                f.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                f.cmd,
                f.cgroup,
            ),
        });
        opened += 1;
    }
    opened
}

/// Every node, periodic. First pass ~30 s after boot (the raw proxy's own
/// listeners are up by then, so they are correctly attributed to self), then
/// every `HIVE_LISTENER_AUDIT_SECS` (default 300). `leader` is evaluated per
/// pass so an incident is raised from whichever node currently owns the
/// control plane.
pub fn spawn(cloud: Arc<CloudState>, leader: impl Fn(&Arc<CloudState>) -> bool + Send + 'static) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            let node = cloud.node_name.clone();
            let report = match tokio::task::spawn_blocking(move || audit_once(&node)).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "listener audit: pass panicked");
                    tokio::time::sleep(Duration::from_secs(interval_secs())).await;
                    continue;
                }
            };
            for f in &report.foreign {
                tracing::warn!(
                    port = f.port,
                    family = f.family,
                    pid = f.pid,
                    cmd = %f.cmd,
                    cgroup = %f.cgroup,
                    "listener audit: FOREIGN wildcard listener inside the internet-open \
                     published-port range (not this process) -- world-reachable through \
                     the security group; kill it or bind it to 127.0.0.1"
                );
            }
            if !report.foreign.is_empty() && leader(&cloud) {
                let n = raise_incidents(&cloud, &report);
                if n > 0 {
                    tracing::warn!(opened = n, "listener audit: incidents opened");
                }
            }
            tracing::debug!(
                own = report.own_in_range,
                foreign = report.foreign.len(),
                elapsed_ms = report.elapsed_ms,
                supported = report.supported,
                "listener audit: pass complete"
            );
            *last_slot().write() = Some(report);
            tokio::time::sleep(Duration::from_secs(interval_secs())).await;
        }
    });
}
