// ╔════════════════════════════════════════════════════════════════════════════════╗
// ║                          ⚠ MODULE IN DEVELOPMENT                              ║
// ╚════════════════════════════════════════════════════════════════════════════════╝
// ═══════════════════════════════════════════════════════════════════════════════
// Guardian-DB Administration Panel (TUI)
// ═══════════════════════════════════════════════════════════════════════════════
//
// Visual panel for inspecting, managing, and monitoring Guardian-DB.
// Phase 1.1: Application scaffold with state machine, base layout, and log capture.
//
// Usage (requires the `sentinel` feature):
//   # Embedded mode — the panel owns the data-dir (redb lock):
//   cargo run --features sentinel --bin guardian-sentinel -- --data-dir ./my_db
//   # RPC mode — attaches to a live instance served by guardian-sentinel-server:
//   cargo run --features sentinel --bin guardian-sentinel -- --connect 127.0.0.1:15433
// ═══════════════════════════════════════════════════════════════════════════════

use guardian_db::guardian::GuardianDB;
// Data-access seam: EmbeddedSource (owns the storage) and AdminClient (RPC) both
// implement the same AdminSource, so the panel consumes both in a unified way.
use guardian_db::sentinel::{
    AclSummary, AdminClient, AdminContext, AdminSource, BlobContent, BlobSummary, CrdtHead,
    DocEntry, EmbeddedSource, KvEntry, LatencyStats, LogEntry, PeerSummary, RelayStatus,
    ThroughputStats, TopoLink,
};
// To consume the seam's event stream (.next()).
use futures::StreamExt;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Sparkline, Wrap,
    },
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tracing_subscriber::fmt::MakeWriter;

/// An event captured from the `EventBus` for the explorer (feature 8).
#[derive(Debug, Clone)]
struct EventRecord {
    /// Formatted arrival time (HH:MM:SS) for display.
    ts: String,
    /// Arrival instant, for the events/second computation.
    at: Instant,
    kind: String,
    detail: String,
    /// Structured fields propagated from `AdminEvent` (B1). They feed the
    /// peers-per-store / stores-per-peer aggregations (B2), sync duration (B3) and top peers.
    store: Option<String>,
    peer: Option<String>,
    heads_synced: Option<usize>,
    duration_ms: Option<u64>,
}

/// Event kinds offered in the explorer filter (index 0 = all).
const EVENT_KINDS: [&str; 8] = [
    "all",
    "sync",
    "peer_connected",
    "peer_disconnected",
    "store_updated",
    "sync_completed",
    "sync_error",
    "database_created",
];

// ═══════════════════════════════════════════════════════════
// Log Capture — redirects tracing to the status bar
// ═══════════════════════════════════════════════════════════

#[derive(Clone)]
struct LogBuffer {
    last_line: Arc<StdMutex<String>>,
}

impl LogBuffer {
    fn new() -> Self {
        Self {
            last_line: Arc::new(StdMutex::new(String::new())),
        }
    }

    fn get_last(&self) -> String {
        self.last_line.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

struct LogWriter {
    buf: Vec<u8>,
    last_line: Arc<StdMutex<String>>,
}

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        if let Ok(s) = std::str::from_utf8(&self.buf) {
            let trimmed = s.trim();
            if !trimmed.is_empty()
                && let Ok(mut last) = self.last_line.lock()
            {
                *last = trimmed.to_string();
            }
        }
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            buf: Vec::new(),
            last_line: Arc::clone(&self.last_line),
        }
    }
}

// ═══════════════════════════════════════════════════════════
// State Machine — panel screens
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum Screen {
    /// Initial loading / DB connection screen
    Connecting,
    /// Overview: store list, metrics, node info
    Dashboard,
    /// Details of a selected store
    StoreDetail { store_address: String },
    /// EventLog inspector
    EventLogInspector { log_name: String },
    /// KeyValue inspector
    KeyValueInspector { kv_name: String },
    /// Document store inspector (feature 1.3 / B4)
    DocumentInspector { store_name: String },
    /// Access Control manager
    AccessControlManager,
    /// Details of an access controller
    AccessControlDetail { controller_id: String },
    /// P2P replication monitor
    ReplicationMonitor,
    /// Details of a peer
    PeerDetail { node_id: String },
    /// Network topology viewer
    NetworkTopology,
    /// EventBus explorer
    EventBusExplorer,
    /// Keystore manager
    KeystoreManager,
    /// Details of a key
    KeyDetail { key_id: String },
    /// BlobStore browser
    BlobBrowser,
    /// Details of a blob
    BlobDetail { hash: String },
    /// Failed to open GuardianDB (e.g. data-dir locked by another process)
    ConnectionFailed { message: String },
}

// ═══════════════════════════════════════════════════════════
// Store Info — metadata collected from open stores
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq)]
enum SyncStatus {
    Synced,
    Syncing,
    Error,
}

impl SyncStatus {
    #[allow(dead_code)]
    fn label(&self) -> &str {
        match self {
            SyncStatus::Synced => "synced",
            SyncStatus::Syncing => "syncing",
            SyncStatus::Error => "error",
        }
    }

    fn color(&self) -> Color {
        match self {
            SyncStatus::Synced => Color::Green,
            SyncStatus::Syncing => Color::Yellow,
            SyncStatus::Error => Color::Red,
        }
    }

    fn icon(&self) -> &str {
        match self {
            SyncStatus::Synced => "●",
            SyncStatus::Syncing => "◐",
            SyncStatus::Error => "✗",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StoreFilter {
    All,
    EventLog,
    KeyValue,
    Document,
}

impl StoreFilter {
    fn next(&self) -> Self {
        match self {
            StoreFilter::All => StoreFilter::EventLog,
            StoreFilter::EventLog => StoreFilter::KeyValue,
            StoreFilter::KeyValue => StoreFilter::Document,
            StoreFilter::Document => StoreFilter::All,
        }
    }

    fn label(&self) -> &str {
        match self {
            StoreFilter::All => "All",
            StoreFilter::EventLog => "EventLog",
            StoreFilter::KeyValue => "KeyValue",
            StoreFilter::Document => "Document",
        }
    }

    fn matches(&self, store_type: &str) -> bool {
        match self {
            StoreFilter::All => true,
            StoreFilter::EventLog => store_type == "eventlog",
            StoreFilter::KeyValue => store_type == "keyvalue",
            StoreFilter::Document => store_type == "document",
        }
    }
}

/// Sort criterion for the Blob Browser (feature 9.1). Sorting by date still
/// depends on an addition timestamp, which iroh-blobs does not expose.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BlobSort {
    Hash,
    Size,
}

impl BlobSort {
    fn next(&self) -> Self {
        match self {
            BlobSort::Hash => BlobSort::Size,
            BlobSort::Size => BlobSort::Hash,
        }
    }

    fn label(&self) -> &str {
        match self {
            BlobSort::Hash => "hash",
            BlobSort::Size => "size",
        }
    }
}

/// Formats a key's lifecycle metadata for the detail modal (D2): type, status
/// (active vs. rotated) and age. Empty line if not tracked.
fn format_key_meta(info: &guardian_db::sentinel::KeyInfo) -> String {
    match (info.created_at, info.rotated_count) {
        (Some(created), Some(rotated)) => {
            let kind = info.kind.as_deref().unwrap_or("ed25519");
            let status = if rotated > 0 {
                format!("rotated {rotated}x")
            } else {
                "active (never rotated)".to_string()
            };
            let age = {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let secs = now.saturating_sub(created);
                if secs < 3600 {
                    format!("{}min", secs / 60)
                } else if secs < 86_400 {
                    format!("{}h", secs / 3600)
                } else {
                    format!("{}d", secs / 86_400)
                }
            };
            format!("Type: {kind} · Status: {status} · Age: {age}\n")
        }
        _ => "Type/status: (metadata not tracked by this keystore)\n".to_string(),
    }
}

/// Formats a byte count in a human-readable way (B/KB/MB/GB).
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KB", n as f64 / KB as f64),
        1_048_576..=1_073_741_823 => format!("{:.1} MB", n as f64 / MB as f64),
        _ => format!("{:.2} GB", n as f64 / GB as f64),
    }
}

#[derive(Debug, Clone)]
struct StoreInfo {
    address: String,
    store_type: String,
    entry_count: usize,
    db_name: String,
    sync_status: SyncStatus,
    replication_progress: usize,
    replication_max: usize,
    #[allow(dead_code)]
    buffered: usize,
}

// ═══════════════════════════════════════════════════════════
// Notification — temporary feedback to the user
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct Notification {
    message: String,
    is_error: bool,
    created_at: Instant,
}

impl Notification {
    fn success(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            is_error: false,
            created_at: Instant::now(),
        }
    }

    #[allow(dead_code)]
    fn error(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            is_error: true,
            created_at: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= 5
    }
}

// ═══════════════════════════════════════════════════════════
// Actions and confirmation — mutations executed via the seam (R3)
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
enum PendingAction {
    KvDelete {
        store: String,
        key: String,
    },
    AclGrant {
        store: String,
        role: String,
        key_id: String,
    },
    AclRevoke {
        store: String,
        role: String,
        key_id: String,
    },
    PeerSync {
        node_id: String,
    },
    AclCreate {
        controller_type: String,
        name: String,
        admin_keys: Vec<String>,
        write_keys: Vec<String>,
    },
    StoreCreate {
        kind: String,
        name: String,
        replicate: bool,
        local_only: bool,
        read_only: bool,
        acl_address: Option<String>,
    },
    /// Show this node's identity (NodeId + addresses) for sharing (G3.1).
    ShowIdentity,
    /// Generate sharing tickets for the selected store (G3.2).
    ShareStore {
        name: String,
    },
    /// Import a store from a DocTicket (G3.3).
    StoreImport {
        kind: String,
        name: String,
        ticket: String,
        read_only: bool,
    },
    /// Close a store (releases the session; reopens on restart) (G2.1).
    StoreClose {
        name: String,
    },
    /// Drop a store (deletes data + registry) (G2.2).
    StoreDrop {
        name: String,
    },
    /// Append an entry to an EventLog (G2.5).
    EventLogAppend {
        store: String,
        data: String,
    },
    /// Put a document into a Document store (G2.6).
    DocPut {
        store: String,
        id: String,
        json: String,
    },
    /// Delete a document from a Document store (G2.6).
    DocDelete {
        store: String,
        id: String,
    },
    ShowHeads {
        store: String,
    },
    ShowBlob {
        hash: String,
    },
    ShowDoc {
        store: String,
        id: String,
    },
    BlobAdd {
        path: String,
    },
    BlobExport {
        hash: String,
        path: String,
    },
    BlobDelete {
        hash: String,
    },
    KvPut {
        store: String,
        key: String,
        value: String,
    },
    KvExport {
        path: String,
    },
    ShowPeer {
        node_id: String,
    },
    ShowKey {
        key_id: String,
    },
    KeystoreGenerate {
        key_id: String,
    },
}

#[derive(Debug, Clone)]
struct ConfirmPrompt {
    message: String,
    action: PendingAction,
}

/// A single-field text input prompt (e.g. typing a key_id).
#[derive(Debug, Clone)]
struct InputPrompt {
    label: String,
    buffer: String,
    kind: InputKind,
}

/// What to do with the typed text when the input is confirmed (Enter).
#[derive(Debug, Clone)]
enum InputKind {
    AclGrant {
        store: String,
        role: String,
    },
    AclRevoke {
        store: String,
        role: String,
    },
    PeerConnect,
    BlobAddPath,
    BlobExportPath {
        hash: String,
    },
    /// Generate a new key in the keystore; buffer is the key ID.
    KeystoreGenerate,
    /// Edit the value of an existing key (buffer pre-filled).
    KvEdit {
        store: String,
        key: String,
    },
    /// Create a new key; buffer in `key=value` format.
    KvCreate {
        store: String,
    },
    /// Export all keys as JSON to a local file.
    KvExportPath,
    /// Set the EventLog's logical (Lamport) clock range; buffer in `min-max`
    /// format (endpoints optional). Empty clears the filter (feature 2.3, B5).
    LogClockRange,
    /// Append an entry to an EventLog; buffer is the payload (text) (G2.5).
    EventLogAppend {
        store: String,
    },
    /// New document in a Document store; buffer in `id={json}` format (G2.6).
    DocCreate {
        store: String,
    },
}

/// An inclusive clock range with optional endpoints: `(min, max)`, either side
/// `None` meaning "unbounded on that end".
type ClockBounds = (Option<u64>, Option<u64>);

/// Parses a logical clock range typed as `min-max` (endpoints optional):
/// `"5-20"`, `"5-"`, `"-20"`, `"5"` (exact) or `""` (clear). Returns
/// `Ok(None)` to clear, `Ok(Some((min,max)))` to apply, `Err` if invalid.
fn parse_clock_range(raw: &str) -> Result<Option<ClockBounds>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let parse_end = |s: &str| -> Result<Option<u64>, String> {
        let s = s.trim();
        if s.is_empty() {
            Ok(None)
        } else {
            s.parse::<u64>()
                .map(Some)
                .map_err(|_| format!("invalid clock value: '{s}'"))
        }
    };
    let (min, max) = match t.split_once('-') {
        Some((lo, hi)) => (parse_end(lo)?, parse_end(hi)?),
        None => {
            // A single number = exact clock (min == max).
            let v = parse_end(t)?;
            (v, v)
        }
    };
    if let (Some(a), Some(b)) = (min, max)
        && a > b
    {
        return Err(format!("invalid range: {a} > {b}"));
    }
    if min.is_none() && max.is_none() {
        return Ok(None);
    }
    Ok(Some((min, max)))
}

/// A persistent info box (e.g. a manifest hash to copy). Stays on screen until
/// the user dismisses it (Esc/Enter), unlike the ephemeral notification.
#[derive(Debug, Clone)]
struct InfoModal {
    title: String,
    body: String,
}

/// Steps of the controller creation wizard.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WizardStep {
    Type,
    Name,
    Admin,
    Write,
    Confirm,
}

/// Controller types offered by the wizard (all map to SimpleAccessController
/// today, but the type is recorded in the manifest).
const CTRL_TYPES: [&str; 3] = ["simple", "guardian", "iroh"];

/// Page block size of the EventLog inspector (feature 2.1). When scrolling to
/// the top (oldest entry loaded), the panel fetches one more block.
const EVENTLOG_PAGE: usize = 500;

/// State of the multi-step controller creation wizard (feature 4.4).
#[derive(Debug, Clone)]
struct ControllerWizard {
    step: WizardStep,
    type_idx: usize,
    name: String,
    admin_keys: String,
    write_keys: String,
    /// Text buffer of the active step (committed to the field on advance).
    buffer: String,
}

impl ControllerWizard {
    fn new() -> Self {
        Self {
            step: WizardStep::Type,
            type_idx: 0,
            name: String::new(),
            admin_keys: String::new(),
            write_keys: String::new(),
            buffer: String::new(),
        }
    }

    fn controller_type(&self) -> &str {
        CTRL_TYPES[self.type_idx]
    }

    fn parse_keys(raw: &str) -> Vec<String> {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Store types creatable from the TUI (G1) + plain-language description.
const STORE_KINDS: [(&str, &str); 3] = [
    ("eventlog", "append-only log (event history, immutable)"),
    ("keyvalue", "key→value pairs (like a dictionary)"),
    ("document", "JSON documents with an id (like a collection)"),
];

/// Steps of the store creation wizard (G1.4 + G2.3 ACL).
#[derive(Debug, Clone, Copy, PartialEq)]
enum StoreWizardStep {
    Kind,
    Name,
    Options,
    Acl,
    Confirm,
}

/// State of the "New store" wizard (G1). The options have sensible defaults:
/// replicate on, local_only/read_only off.
#[derive(Debug, Clone)]
struct StoreWizard {
    step: StoreWizardStep,
    kind_idx: usize,
    name: String,
    replicate: bool,
    local_only: bool,
    read_only: bool,
    /// Index of the selected toggle in the options step (0..3).
    opt_idx: usize,
    /// Address of an access controller to attach (optional) (G2.3).
    acl: String,
}

impl StoreWizard {
    fn new() -> Self {
        Self {
            step: StoreWizardStep::Kind,
            kind_idx: 0,
            name: String::new(),
            replicate: true,
            local_only: false,
            read_only: false,
            opt_idx: 0,
            acl: String::new(),
        }
    }

    fn kind(&self) -> &'static str {
        STORE_KINDS[self.kind_idx].0
    }
}

/// Types importable via DocTicket (G3.3) — iroh-docs stores only.
const IMPORT_KINDS: [(&str, &str); 2] = [
    ("keyvalue", "shared key→value pairs"),
    ("document", "shared document collection"),
];

/// Steps of the "Import store" wizard (G3.3).
#[derive(Debug, Clone, Copy, PartialEq)]
enum ImportStep {
    Kind,
    Name,
    Ticket,
}

/// State of the import wizard (G3.3): paste a peer's DocTicket and open a local
/// store that joins the shared namespace.
#[derive(Debug, Clone)]
struct ImportWizard {
    step: ImportStep,
    kind_idx: usize,
    name: String,
    ticket: String,
    read_only: bool,
}

impl ImportWizard {
    fn new() -> Self {
        Self {
            step: ImportStep::Kind,
            kind_idx: 0,
            name: String::new(),
            ticket: String::new(),
            read_only: false,
        }
    }

    fn kind(&self) -> &'static str {
        IMPORT_KINDS[self.kind_idx].0
    }
}

// ═══════════════════════════════════════════════════════════
// App State — the application's central state
// ═══════════════════════════════════════════════════════════

struct App {
    screen: Screen,
    screen_history: Vec<Screen>,
    should_quit: bool,
    log_buffer: LogBuffer,
    notification: Option<Notification>,
    started_at: Instant,

    // DB data
    node_id: String,
    /// Source label for the header: "dir: ./x" (embedded) or "rpc: 127.0.0.1:…".
    source_label: String,
    stores: Vec<StoreInfo>,
    filtered_indices: Vec<usize>,
    store_list_state: ListState,
    store_filter: StoreFilter,
    /// Current sort criterion of the Blob Browser (feature 9.1).
    blob_sort: BlobSort,

    // Inspection-screen data (reloaded via the seam on entering the screen)
    kv_entries: Vec<KvEntry>,
    log_entries: Vec<LogEntry>,
    doc_entries: Vec<DocEntry>,
    peers: Vec<PeerSummary>,
    blobs: Vec<BlobSummary>,
    acls: Vec<AclSummary>,
    keystore_keys: Vec<String>,
    topo: Vec<TopoLink>,
    /// Home-relay status (C2), global latency percentiles (C1), aggregate
    /// throughput (D1) and known-but-offline peers (C3), loaded with the topology.
    relays: Vec<RelayStatus>,
    latency: Option<LatencyStats>,
    throughput: Option<ThroughputStats>,
    discovered: Vec<String>,
    inspector_state: ListState,

    // EventBus Explorer (feature 8)
    /// Display buffer of events (drained from `incoming` each tick).
    events: VecDeque<EventRecord>,
    /// Shared buffer where the streaming task pushes events.
    incoming: Arc<StdMutex<VecDeque<EventRecord>>>,
    /// Freezes the display (the background buffer keeps filling).
    event_paused: bool,
    /// Automatically scrolls to the most recent event.
    event_follow: bool,
    /// Index of the active kind filter in `EVENT_KINDS`.
    event_kind_filter: usize,
    /// Search filter of the EventLog inspector (None = no filter).
    search: Option<String>,
    /// True while the search field is being edited (keys go to the query).
    searching: bool,
    /// Logical (Lamport) clock range filter on the EventLog (feature 2.3, B5).
    /// `(min, max)` inclusive; each endpoint is optional.
    log_clock_range: Option<(Option<u64>, Option<u64>)>,
    /// Signals to the event loop that the current screen needs to reload its data.
    needs_fetch: bool,
    /// Signals that the EventLog inspector should page one more block (feature 2.1),
    /// loading entries older than the oldest already loaded.
    needs_load_more: bool,
    /// True while the last loaded block came back full — there may be older
    /// history to page. False once the log has ended.
    log_has_more: bool,
    /// Active confirmation dialog (destructive action awaiting y/N).
    confirm: Option<ConfirmPrompt>,
    /// Active text input prompt (e.g. typing a key_id for grant/revoke).
    input: Option<InputPrompt>,
    /// Persistent info box (e.g. a just-created manifest hash).
    info_modal: Option<InfoModal>,
    /// Contextual help for the current screen (G4.1). Overlay larger than `info_modal`.
    help_modal: Option<InfoModal>,
    /// Active controller creation wizard (feature 4.4).
    wizard: Option<ControllerWizard>,
    /// Active store creation wizard (G1.4).
    store_wizard: Option<StoreWizard>,
    /// Active store import wizard (G3.3).
    import_wizard: Option<ImportWizard>,
    /// Confirmed action awaiting async execution in the event loop.
    pending_action: Option<PendingAction>,

    // Network counters
    peers_online: usize,
    syncs_total: u64,
    sync_errors: u64,
    has_updates: Arc<AtomicBool>,

    /// Timestamp of the last refresh; used by a slow periodic fallback (the
    /// reactive refresh comes from the seam's event stream, in both modes).
    last_refresh: Instant,

    /// Audit trail of this session's administration actions (G5.2):
    /// `(time, description)` of mutations (create/drop/grant/revoke/rotate/
    /// delete/import). Client-side, capped; visible via the `l` key on the Dashboard.
    audit_log: VecDeque<(String, String)>,

    /// Splash logo (guardian-sentinel-logo) decoded once — used in the block
    /// fallback when the terminal has no graphics protocol.
    logo_img: Option<image::DynamicImage>,

    /// Small header emblem (sentinel-small-logo) decoded once.
    header_img: Option<image::DynamicImage>,

    /// `true` when the terminal has a graphics protocol (Sixel/Kitty/iTerm2) → uses
    /// `ratatui-image` (crisp). `false` → quadrant-block fallback.
    graphics: bool,

    /// `ratatui-image` protocols for the splash and header (only when `graphics`).
    /// `RefCell` because `ui()` takes `&App` and the widget mutates the protocol while drawing.
    logo_proto: std::cell::RefCell<Option<ratatui_image::protocol::StatefulProtocol>>,
    header_proto: std::cell::RefCell<Option<ratatui_image::protocol::StatefulProtocol>>,
}

impl App {
    fn new(log_buffer: LogBuffer, source_label: String) -> Self {
        Self {
            screen: Screen::Connecting,
            screen_history: Vec::new(),
            should_quit: false,
            log_buffer,
            notification: None,
            started_at: Instant::now(),

            node_id: String::new(),
            source_label,
            stores: Vec::new(),
            filtered_indices: Vec::new(),
            store_list_state: ListState::default(),
            store_filter: StoreFilter::All,
            blob_sort: BlobSort::Hash,

            kv_entries: Vec::new(),
            log_entries: Vec::new(),
            doc_entries: Vec::new(),
            peers: Vec::new(),
            blobs: Vec::new(),
            acls: Vec::new(),
            keystore_keys: Vec::new(),
            topo: Vec::new(),
            relays: Vec::new(),
            latency: None,
            throughput: None,
            discovered: Vec::new(),
            inspector_state: ListState::default(),

            events: VecDeque::new(),
            incoming: Arc::new(StdMutex::new(VecDeque::new())),
            event_paused: false,
            event_follow: true,
            event_kind_filter: 0,
            search: None,
            searching: false,
            log_clock_range: None,
            needs_fetch: false,
            needs_load_more: false,
            log_has_more: false,
            confirm: None,
            input: None,
            info_modal: None,
            help_modal: None,
            wizard: None,
            store_wizard: None,
            import_wizard: None,
            pending_action: None,

            peers_online: 0,
            syncs_total: 0,
            sync_errors: 0,
            has_updates: Arc::new(AtomicBool::new(false)),

            last_refresh: Instant::now(),
            audit_log: VecDeque::new(),
            logo_img: image::load_from_memory(include_bytes!(
                "../../docs/guardian-sentinel-logo.png"
            ))
            .ok(),
            header_img: image::load_from_memory(include_bytes!(
                "../../docs/sentinel-small-logo.png"
            ))
            .ok(),
            graphics: false,
            logo_proto: std::cell::RefCell::new(None),
            header_proto: std::cell::RefCell::new(None),
        }
    }

    /// Records an administration action in the audit trail (G5.2).
    fn audit(&mut self, msg: impl Into<String>) {
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        self.audit_log.push_front((ts, msg.into()));
        while self.audit_log.len() > 200 {
            self.audit_log.pop_back();
        }
    }

    /// Navigates to a new screen, pushing the previous one onto the stack
    fn navigate_to(&mut self, screen: Screen) {
        let current = self.screen.clone();
        self.screen_history.push(current);
        self.screen = screen;
    }

    /// Returns to the previous screen
    fn go_back(&mut self) {
        if let Some(prev) = self.screen_history.pop() {
            self.screen = prev;
        }
    }

    /// Sets a success notification
    fn notify_success(&mut self, msg: impl Into<String>) {
        self.notification = Some(Notification::success(msg));
    }

    /// Sets an error notification
    #[allow(dead_code)]
    fn notify_error(&mut self, msg: impl Into<String>) {
        self.notification = Some(Notification::error(msg));
    }

    /// Clears expired notifications
    fn tick_notifications(&mut self) {
        if let Some(ref n) = self.notification
            && n.is_expired()
        {
            self.notification = None;
        }
    }

    /// Returns the formatted uptime
    fn uptime(&self) -> String {
        let secs = self.started_at.elapsed().as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{h:02}:{m:02}:{s:02}")
    }

    /// Refreshes the store list via the AdminSource seam (embedded or RPC).
    async fn refresh_stores(&mut self, source: &dyn AdminSource) {
        self.last_refresh = Instant::now();

        let summaries = match source.stores_list().await {
            Ok(s) => s,
            Err(e) => {
                self.notify_error(format!("Failed to list stores: {}", e.code));
                return;
            }
        };
        let mut infos = Vec::with_capacity(summaries.len());

        for s in summaries {
            // Replication is handled natively by Iroh; the store no longer exposes
            // progress counters, so stores are reported as synced.
            infos.push(StoreInfo {
                address: s.address,
                store_type: s.store_type,
                entry_count: s.entry_count,
                db_name: s.db_name,
                sync_status: SyncStatus::Synced,
                replication_progress: 0,
                replication_max: 0,
                buffered: 0,
            });
        }

        // Sort: eventlog first, then keyvalue, then document
        infos.sort_by(|a, b| {
            let type_order = |t: &str| match t {
                "eventlog" => 0,
                "keyvalue" => 1,
                "document" => 2,
                _ => 3,
            };
            type_order(&a.store_type)
                .cmp(&type_order(&b.store_type))
                .then_with(|| a.db_name.cmp(&b.db_name))
        });

        self.stores = infos;
        self.apply_filter();
    }

    /// Applies the current filter and recomputes the visible indices
    fn apply_filter(&mut self) {
        self.filtered_indices = self
            .stores
            .iter()
            .enumerate()
            .filter(|(_, s)| self.store_filter.matches(&s.store_type))
            .map(|(i, _)| i)
            .collect();

        // Adjust selection if it fell out of bounds
        if self.filtered_indices.is_empty() {
            self.store_list_state.select(None);
        } else if let Some(sel) = self.store_list_state.selected()
            && sel >= self.filtered_indices.len()
        {
            self.store_list_state
                .select(Some(self.filtered_indices.len() - 1));
        }
    }

    /// Returns the stores filtered by the current selection
    fn filtered_stores(&self) -> Vec<&StoreInfo> {
        self.filtered_indices
            .iter()
            .filter_map(|&i| self.stores.get(i))
            .collect()
    }

    /// Returns the currently selected store (taking the filter into account)
    fn selected_store(&self) -> Option<&StoreInfo> {
        self.store_list_state
            .selected()
            .and_then(|sel| self.filtered_indices.get(sel))
            .and_then(|&i| self.stores.get(i))
    }

    /// Re-sorts `self.blobs` by the current criterion (feature 9.1). Sorting by
    /// size is descending (largest first); by hash is alphabetical.
    fn sort_blobs(&mut self) {
        match self.blob_sort {
            BlobSort::Hash => self.blobs.sort_by(|a, b| a.hash.cmp(&b.hash)),
            BlobSort::Size => self
                .blobs
                .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.hash.cmp(&b.hash))),
        }
    }

    /// Total disk usage of the listed blobs (feature 9.3): sum of sizes.
    fn blobs_total_size(&self) -> u64 {
        self.blobs.iter().map(|b| b.size).sum()
    }

    /// Number of items in the current inspection screen's list (for scrolling).
    fn inspector_len(&self) -> usize {
        match &self.screen {
            Screen::KeyValueInspector { .. } => self.visible_kv_entries().len(),
            Screen::EventLogInspector { .. } => self.visible_log_entries().len(),
            Screen::DocumentInspector { .. } => self.visible_doc_entries().len(),
            Screen::ReplicationMonitor => self.peers.len(),
            Screen::BlobBrowser => self.blobs.len(),
            Screen::AccessControlManager => self.acls.len(),
            Screen::KeystoreManager => self.keystore_keys.len(),
            Screen::NetworkTopology => self.topo.len(),
            Screen::EventBusExplorer => self.visible_events().len(),
            _ => 0,
        }
    }

    /// Moves events from `incoming` into the display buffer (respecting pause),
    /// keeping the ring capped. Called on every event-loop tick.
    fn drain_events(&mut self) {
        if self.event_paused {
            return;
        }
        if let Ok(mut inc) = self.incoming.lock() {
            while let Some(rec) = inc.pop_front() {
                self.events.push_back(rec);
            }
        }
        while self.events.len() > 1000 {
            self.events.pop_front();
        }
    }

    /// Events visible after applying the kind filter and the search (feature 8.2).
    fn visible_events(&self) -> Vec<&EventRecord> {
        let kind = EVENT_KINDS[self.event_kind_filter];
        let q = self.search.as_deref().unwrap_or("").to_lowercase();
        self.events
            .iter()
            .filter(|e| self.event_kind_filter == 0 || e.kind == kind)
            .filter(|e| {
                q.is_empty()
                    || e.kind.to_lowercase().contains(&q)
                    || e.detail.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// Event count by kind (feature 8.3).
    fn event_counts(&self) -> std::collections::BTreeMap<String, usize> {
        let mut m = std::collections::BTreeMap::new();
        for e in &self.events {
            *m.entry(e.kind.clone()).or_insert(0) += 1;
        }
        m
    }

    /// Events per second estimated over the last 5-second window.
    fn events_per_sec(&self) -> f64 {
        let recent = self
            .events
            .iter()
            .filter(|e| e.at.elapsed().as_secs_f64() <= 5.0)
            .count();
        recent as f64 / 5.0
    }

    // ── Replication metrics (feature 5.3/5.4) ──

    fn is_sync_event(kind: &str) -> bool {
        kind == "sync" || kind == "sync_completed"
    }

    /// Per-second buckets of sync activity over the last 60s (recent on the right).
    fn sync_sparkline(&self) -> Vec<u64> {
        let mut buckets = vec![0u64; 60];
        for e in &self.events {
            if Self::is_sync_event(&e.kind) {
                let age = e.at.elapsed().as_secs();
                if age < 60 {
                    buckets[59 - age as usize] += 1;
                }
            }
        }
        buckets
    }

    /// Per-second buckets of *all* events over the last 60s (recent on the right).
    /// Generalizes `sync_sparkline` to the EventBus's total activity (feature 8.3).
    fn event_sparkline(&self) -> Vec<u64> {
        let mut buckets = vec![0u64; 60];
        for e in &self.events {
            let age = e.at.elapsed().as_secs();
            if age < 60 {
                buckets[59 - age as usize] += 1;
            }
        }
        buckets
    }

    /// Syncs in the last minute.
    fn syncs_last_min(&self) -> usize {
        self.events
            .iter()
            .filter(|e| Self::is_sync_event(&e.kind) && e.at.elapsed().as_secs() < 60)
            .count()
    }

    /// Sync errors in the last minute.
    fn sync_errors_last_min(&self) -> usize {
        self.events
            .iter()
            .filter(|e| e.kind == "sync_error" && e.at.elapsed().as_secs() < 60)
            .count()
    }

    /// Seconds since the last sync involving `node_id` (None if none in the buffer).
    /// Uses the structured `peer` field (B1), falling back to the detail text.
    fn peer_last_sync_secs(&self, node_id: &str) -> Option<u64> {
        self.events
            .iter()
            .filter(|e| Self::is_sync_event(&e.kind) && Self::event_mentions_peer(e, node_id))
            .map(|e| e.at.elapsed().as_secs())
            .min()
    }

    /// True if the event involves `node_id`, preferring the structured `peer` field.
    fn event_mentions_peer(e: &EventRecord, node_id: &str) -> bool {
        match &e.peer {
            Some(p) => p == node_id,
            None => e.detail.contains(node_id),
        }
    }

    // ── Aggregations derived from the event stream (B1 → B2/B3, top peers) ──

    /// Top peers by event volume in the buffer (feature 8.3 / A1). Uses the
    /// structured `peer` field; returns `(peer, count)` from highest to lowest.
    fn top_peers(&self, n: usize) -> Vec<(String, usize)> {
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for e in &self.events {
            if let Some(p) = e.peer.as_deref() {
                *counts.entry(p).or_insert(0) += 1;
            }
        }
        let mut v: Vec<(String, usize)> = counts
            .into_iter()
            .map(|(p, c)| (p.to_string(), c))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Peers observed syncing the store `store_addr` (feature 1.3, via B1).
    /// An *observed* view since the panel opened — not an instantaneous truth.
    /// Returns `(peer, syncs, seconds since the last)` ordered by activity.
    fn peers_for_store(&self, store_addr: &str) -> Vec<(String, usize, u64)> {
        let mut agg: std::collections::HashMap<&str, (usize, u64)> =
            std::collections::HashMap::new();
        for e in &self.events {
            if !Self::is_sync_event(&e.kind) {
                continue;
            }
            if e.store.as_deref() != Some(store_addr) {
                continue;
            }
            if let Some(p) = e.peer.as_deref() {
                let age = e.at.elapsed().as_secs();
                let slot = agg.entry(p).or_insert((0, age));
                slot.0 += 1;
                slot.1 = slot.1.min(age);
            }
        }
        let mut v: Vec<(String, usize, u64)> = agg
            .into_iter()
            .map(|(p, (c, last))| (p.to_string(), c, last))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// Stores observed shared with the peer `node_id` (feature 5.2, via B1).
    /// The inverse of `peers_for_store`. Returns `(store, syncs)` by activity.
    fn stores_for_peer(&self, node_id: &str) -> Vec<(String, usize)> {
        let mut agg: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for e in &self.events {
            if !Self::is_sync_event(&e.kind) {
                continue;
            }
            if e.peer.as_deref() != Some(node_id) {
                continue;
            }
            if let Some(s) = e.store.as_deref() {
                *agg.entry(s).or_insert(0) += 1;
            }
        }
        let mut v: Vec<(String, usize)> =
            agg.into_iter().map(|(s, c)| (s.to_string(), c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// Average and last sync duration (ms) of the `sync_completed` events in the
    /// buffer (feature 5.3, via B1). `None` if no completed sync was observed.
    fn sync_duration_stats(&self) -> Option<(u64, u64)> {
        let mut sum = 0u64;
        let mut count = 0u64;
        let mut last = None;
        for e in &self.events {
            if e.kind == "sync_completed"
                && let Some(d) = e.duration_ms
            {
                sum += d;
                count += 1;
                last = Some(d);
            }
        }
        last.map(|l| (sum / count.max(1), l))
    }

    /// Approximation of "syncs in progress" (feature 5.3): head exchanges (`sync`,
    /// = start) without a matching `sync_completed` in the recent window. Since the
    /// core exposes no progress state, this is an honest estimate by difference.
    fn syncs_in_progress(&self) -> usize {
        let window = 30; // seconds
        let mut starts = 0i64;
        let mut completions = 0i64;
        for e in &self.events {
            if e.at.elapsed().as_secs() >= window {
                continue;
            }
            match e.kind.as_str() {
                "sync" => starts += 1,
                "sync_completed" => completions += 1,
                _ => {}
            }
        }
        (starts - completions).max(0) as usize
    }

    /// KV keys visible after applying the search filter (feature 3.3).
    fn visible_kv_entries(&self) -> Vec<&KvEntry> {
        match &self.search {
            Some(q) if !q.is_empty() => {
                let ql = q.to_lowercase();
                self.kv_entries
                    .iter()
                    .filter(|e| {
                        e.key.to_lowercase().contains(&ql)
                            || e.value_utf8.to_lowercase().contains(&ql)
                    })
                    .collect()
            }
            _ => self.kv_entries.iter().collect(),
        }
    }

    /// Documents visible after applying the search filter (B4). Filters by id + JSON.
    fn visible_doc_entries(&self) -> Vec<&DocEntry> {
        match &self.search {
            Some(q) if !q.is_empty() => {
                let ql = q.to_lowercase();
                self.doc_entries
                    .iter()
                    .filter(|d| {
                        d.id.to_lowercase().contains(&ql)
                            || d.value_utf8.to_lowercase().contains(&ql)
                    })
                    .collect()
            }
            _ => self.doc_entries.iter().collect(),
        }
    }

    /// EventLog entries visible after applying search (2.3) + logical clock range
    /// (2.3/B5). The two filters combine (AND).
    fn visible_log_entries(&self) -> Vec<&LogEntry> {
        let ql = self
            .search
            .as_deref()
            .filter(|q| !q.is_empty())
            .map(|q| q.to_lowercase());
        self.log_entries
            .iter()
            .filter(|e| match &ql {
                Some(q) => log_matches(e, q),
                None => true,
            })
            .filter(|e| self.log_entry_in_clock_range(e))
            .collect()
    }

    /// True if the entry is within the active logical clock range (or no range).
    fn log_entry_in_clock_range(&self, e: &LogEntry) -> bool {
        match self.log_clock_range {
            None => true,
            Some((min, max)) => {
                min.is_none_or(|m| e.clock_time >= m) && max.is_none_or(|m| e.clock_time <= m)
            }
        }
    }

    /// Reloads the current inspection screen's data via the seam. Called by the
    /// event loop when `needs_fetch` is set (on entering the screen or on refresh).
    async fn load_screen(&mut self, source: &dyn AdminSource) {
        self.needs_fetch = false;
        self.inspector_state.select(None);
        // A (re)load resets the search and logical-clock filters.
        self.search = None;
        self.searching = false;
        self.log_clock_range = None;

        match self.screen.clone() {
            Screen::KeyValueInspector { kv_name } => match source.kv_entries(&kv_name).await {
                Ok(v) => self.kv_entries = v,
                Err(e) => self.notify_error(format!("kv: {}", e.code)),
            },
            Screen::DocumentInspector { store_name } => match source.docs_list(&store_name).await {
                Ok(v) => self.doc_entries = v,
                Err(e) => self.notify_error(format!("docs: {}", e.code)),
            },
            Screen::EventLogInspector { log_name } => {
                self.needs_load_more = false;
                match source
                    .eventlog_entries(&log_name, Some(EVENTLOG_PAGE), None)
                    .await
                {
                    Ok(v) => {
                        // A full block suggests there is older history to page.
                        self.log_has_more = v.len() >= EVENTLOG_PAGE;
                        self.log_entries = v;
                    }
                    Err(e) => self.notify_error(format!("eventlog: {}", e.code)),
                }
            }
            Screen::ReplicationMonitor => match source.peers_list().await {
                Ok(v) => self.peers = v,
                Err(e) => self.notify_error(format!("peers: {}", e.code)),
            },
            Screen::BlobBrowser => match source.blobs_list().await {
                Ok(v) => {
                    self.blobs = v;
                    self.sort_blobs();
                }
                Err(e) => self.notify_error(format!("blobs: {}", e.code)),
            },
            Screen::AccessControlManager => match source.acl_list().await {
                Ok(v) => self.acls = v,
                Err(e) => self.notify_error(format!("acl: {}", e.code)),
            },
            Screen::KeystoreManager => match source.keystore_list().await {
                Ok(v) => self.keystore_keys = v,
                Err(e) => self.notify_error(format!("keystore: {}", e.code)),
            },
            Screen::NetworkTopology => {
                match source.net_topology().await {
                    Ok(v) => self.topo = v,
                    Err(e) => self.notify_error(format!("topology: {}", e.code)),
                }
                // Relay (C2), global latency (C1), aggregate throughput (D1) and
                // known-but-offline peers (C3).
                self.relays = source.net_relay().await.unwrap_or_default();
                self.latency = source.node_latency().await.ok();
                self.throughput = source.node_throughput().await.ok();
                self.discovered = source.net_discovered().await.unwrap_or_default();
            }
            _ => {}
        }

        if self.inspector_len() > 0 {
            self.inspector_state.select(Some(0));
        }
    }

    /// Pages one more block of *older* entries of the current EventLog (feature
    /// 2.1). Uses the hash of the oldest already-loaded entry as the `before`
    /// cursor and prepends the returned block, keeping the user's selection on
    /// the same entry.
    async fn load_more_log_entries(&mut self, source: &dyn AdminSource) {
        self.needs_load_more = false;
        let Screen::EventLogInspector { log_name } = self.screen.clone() else {
            return;
        };
        if !self.log_has_more || self.log_entries.is_empty() {
            return;
        }
        // The oldest loaded entry is at the top (index 0).
        let cursor = self.log_entries[0].hash.clone();
        if cursor.is_empty() {
            self.log_has_more = false;
            return;
        }
        let older = match source
            .eventlog_entries(&log_name, Some(EVENTLOG_PAGE), Some(&cursor))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                self.notify_error(format!("eventlog: {}", e.code));
                return;
            }
        };
        self.log_has_more = older.len() >= EVENTLOG_PAGE;
        if older.is_empty() {
            return;
        }
        let added = older.len();
        // Prepend the old block and renumber the display indices (0 = oldest).
        let mut merged = older;
        merged.append(&mut self.log_entries);
        for (i, e) in merged.iter_mut().enumerate() {
            e.index = i;
        }
        self.log_entries = merged;
        // Keep the selected item pointing at the same entry (shifted down).
        if let Some(sel) = self.inspector_state.selected() {
            self.inspector_state.select(Some(sel + added));
        }
        self.notify_success(format!("+{added} older entries loaded"));
    }

    /// Runs the confirmed action (mutation via the seam) and reloads the screen.
    async fn run_pending_action(&mut self, source: &dyn AdminSource) {
        let Some(action) = self.pending_action.take() else {
            return;
        };
        match action {
            PendingAction::KvDelete { store, key } => match source.kv_delete(&store, &key).await {
                Ok(()) => {
                    self.notify_success(format!("Key '{key}' deleted"));
                    self.audit(format!("deleted KV key '{key}' in '{store}'"));
                    self.needs_fetch = true;
                }
                Err(e) => self.notify_error(format!("delete: {}", e.code)),
            },
            PendingAction::AclGrant {
                store,
                role,
                key_id,
            } => match source.acl_grant(&store, &role, &key_id).await {
                Ok(()) => {
                    self.notify_success(format!("Granted '{role}' to {key_id}"));
                    self.audit(format!("granted '{role}' to {key_id} in '{store}'"));
                    self.needs_fetch = true;
                }
                Err(e) => self.notify_error(format!("grant: {}", e.code)),
            },
            PendingAction::AclRevoke {
                store,
                role,
                key_id,
            } => match source.acl_revoke(&store, &role, &key_id).await {
                Ok(()) => {
                    self.notify_success(format!("Revoked '{role}' from {key_id}"));
                    self.audit(format!("revoked '{role}' from {key_id} in '{store}'"));
                    self.needs_fetch = true;
                }
                Err(e) => self.notify_error(format!("revoke: {}", e.code)),
            },
            PendingAction::PeerSync { node_id } => match source.peer_sync(&node_id).await {
                Ok(()) => {
                    let short: String = node_id.chars().take(12).collect();
                    self.notify_success(format!("Sync started with {short}…"));
                    self.needs_fetch = true;
                }
                Err(e) => self.notify_error(format!("sync: {}", e.code)),
            },
            PendingAction::AclCreate {
                controller_type,
                name,
                admin_keys,
                write_keys,
            } => match source
                .acl_create(&controller_type, &name, admin_keys, write_keys)
                .await
            {
                Ok(hash) => {
                    self.audit(format!(
                        "created ACL controller '{name}' ({controller_type})"
                    ));
                    // Persistent modal so the user can read/copy the full hash.
                    self.info_modal = Some(InfoModal {
                        title: "Controller created".into(),
                        body: format!(
                            "Type: {controller_type}\nManifest (share this hash):\n\n{hash}"
                        ),
                    });
                }
                Err(e) => self.notify_error(format!("create: {}", e.code)),
            },
            PendingAction::StoreCreate {
                kind,
                name,
                replicate,
                local_only,
                read_only,
                acl_address,
            } => {
                let opts = guardian_db::sentinel::StoreCreateOpts {
                    replicate,
                    local_only,
                    read_only,
                    acl_address,
                };
                match source.stores_create(&kind, &name, opts).await {
                    Ok(address) => {
                        self.notify_success(format!("Store '{name}' ({kind}) created"));
                        self.audit(format!("created store '{name}' ({kind})"));
                        // Return to the Dashboard and reload the store list.
                        self.info_modal = Some(InfoModal {
                            title: "Store created".into(),
                            body: format!(
                                "Name: {name}\nType: {kind}\nAddress:\n\n{address}\n\n\
                                 (Reopens automatically on restart.)"
                            ),
                        });
                        self.has_updates.store(true, Ordering::Relaxed);
                    }
                    Err(e) => self.notify_error(format!("store: {}", e.code)),
                }
            }
            PendingAction::ShowIdentity => match source.node_identity().await {
                Ok(id) => {
                    let addrs = if id.addresses.is_empty() {
                        "(no address bound yet)".to_string()
                    } else {
                        id.addresses.join("\n  ")
                    };
                    self.info_modal = Some(InfoModal {
                        title: "My identity (share)".into(),
                        body: format!(
                            "NodeId (share it so others can connect to you):\n\n{}\n\nAddresses:\n  {}",
                            id.node_id, addrs
                        ),
                    });
                }
                Err(e) => self.notify_error(format!("identity: {}", e.code)),
            },
            PendingAction::ShareStore { name } => match source.stores_share(&name).await {
                Ok(t) => {
                    self.audit(format!("generated sharing tickets for '{name}'"));
                    self.info_modal = Some(InfoModal {
                        title: format!("Share '{name}'"),
                        body: format!(
                            "Send one of these tickets to a peer to import the store:\n\n\
                             READ (cannot write):\n{}\n\n\
                             WRITE (can write):\n{}\n\n\
                             The peer uses 'i' on the Dashboard to import.",
                            t.read, t.write
                        ),
                    });
                }
                Err(e) => self.notify_error(format!("share: {}", e.code)),
            },
            PendingAction::StoreImport {
                kind,
                name,
                ticket,
                read_only,
            } => match source.stores_import(&kind, &name, &ticket, read_only).await {
                Ok(address) => {
                    self.notify_success(format!("Store '{name}' imported"));
                    self.audit(format!("imported store '{name}' ({kind}) from a ticket"));
                    self.info_modal = Some(InfoModal {
                        title: "Store imported".into(),
                        body: format!(
                            "Name: {name}\nType: {kind}\nAddress:\n\n{address}\n\n\
                             (Reopens and syncs automatically on restart.)"
                        ),
                    });
                    self.has_updates.store(true, Ordering::Relaxed);
                }
                Err(e) => self.notify_error(format!("import: {}", e.code)),
            },
            PendingAction::StoreClose { name } => match source.stores_close(&name).await {
                Ok(()) => {
                    self.notify_success(format!("Store '{name}' closed (reopens on restart)"));
                    self.audit(format!("closed store '{name}'"));
                    self.has_updates.store(true, Ordering::Relaxed);
                }
                Err(e) => self.notify_error(format!("close: {}", e.code)),
            },
            PendingAction::StoreDrop { name } => match source.stores_drop(&name).await {
                Ok(()) => {
                    self.notify_success(format!("Store '{name}' removed (data deleted)"));
                    self.audit(format!("DROPPED store '{name}' (data deleted)"));
                    self.has_updates.store(true, Ordering::Relaxed);
                }
                Err(e) => self.notify_error(format!("drop: {}", e.code)),
            },
            PendingAction::EventLogAppend { store, data } => {
                match source.eventlog_append(&store, &data).await {
                    Ok(_hash) => {
                        self.notify_success("Entry appended to the log");
                        self.needs_fetch = true;
                    }
                    Err(e) => self.notify_error(format!("append: {}", e.code)),
                }
            }
            PendingAction::DocPut { store, id, json } => {
                match source.docs_put(&store, &id, &json).await {
                    Ok(_) => {
                        self.notify_success(format!("Document '{id}' saved"));
                        self.needs_fetch = true;
                    }
                    Err(e) => self.notify_error(format!("docs: {}", e.code)),
                }
            }
            PendingAction::DocDelete { store, id } => match source.docs_delete(&store, &id).await {
                Ok(()) => {
                    self.notify_success(format!("Document '{id}' deleted"));
                    self.audit(format!("deleted document '{id}' in '{store}'"));
                    self.needs_fetch = true;
                }
                Err(e) => self.notify_error(format!("docs: {}", e.code)),
            },
            PendingAction::ShowHeads { store } => match source.eventlog_heads(&store).await {
                Ok(heads) => {
                    let body = heads_detail(&heads, &self.log_entries);
                    self.info_modal = Some(InfoModal {
                        title: "EventLog heads".into(),
                        body,
                    });
                }
                Err(e) => self.notify_error(format!("heads: {}", e.code)),
            },
            PendingAction::ShowBlob { hash } => match source.blob_get(&hash).await {
                Ok(content) => {
                    self.info_modal = Some(InfoModal {
                        title: "Blob detail".into(),
                        body: blob_detail(&hash, &content),
                    });
                }
                Err(e) => self.notify_error(format!("blob: {}", e.code)),
            },
            PendingAction::ShowDoc { store, id } => match source.docs_get(&store, &id).await {
                Ok(doc) => {
                    self.info_modal = Some(InfoModal {
                        title: format!("Document: {id}"),
                        body: format!("id: {}\nsize: {} b\n\n{}", doc.id, doc.size, doc.value_utf8),
                    });
                }
                Err(e) => self.notify_error(format!("docs: {}", e.code)),
            },
            PendingAction::BlobAdd { path } => match source.blob_add(&path).await {
                Ok(hash) => {
                    self.info_modal = Some(InfoModal {
                        title: "Blob added".into(),
                        body: format!("File: {path}\nHash (share it):\n\n{hash}"),
                    });
                    self.needs_fetch = true;
                }
                Err(e) => self.notify_error(format!("add: {}", e.code)),
            },
            PendingAction::BlobExport { hash, path } => {
                match source.blob_export(&hash, &path).await {
                    Ok(n) => self.notify_success(format!("Exported {n} bytes → {path}")),
                    Err(e) => self.notify_error(format!("export: {}", e.code)),
                }
            }
            PendingAction::BlobDelete { hash } => match source.blob_delete(&hash).await {
                Ok(()) => {
                    let short: String = hash.chars().take(12).collect();
                    self.notify_success(format!("Blob {short}… deleted"));
                    self.audit(format!("deleted blob {short}…"));
                    self.needs_fetch = true;
                }
                Err(e) => self.notify_error(format!("delete: {}", e.code)),
            },
            PendingAction::KvPut { store, key, value } => {
                match source.kv_put(&store, &key, value.into_bytes()).await {
                    Ok(()) => {
                        self.notify_success(format!("Key '{key}' written"));
                        self.needs_fetch = true;
                    }
                    Err(e) => self.notify_error(format!("put: {}", e.code)),
                }
            }
            PendingAction::ShowPeer { node_id } => {
                let topo = source.net_topology().await.unwrap_or_default();
                let link = topo.iter().find(|l| l.node_id == node_id).cloned();
                let peer = self.peers.iter().find(|p| p.node_id == node_id).cloned();
                let shared_stores = self.stores_for_peer(&node_id);
                let body = peer_detail(
                    &node_id,
                    peer.as_ref(),
                    link.as_ref(),
                    &self.events,
                    &shared_stores,
                );
                self.info_modal = Some(InfoModal {
                    title: "Peer detail".into(),
                    body,
                });
            }
            PendingAction::ShowKey { key_id } => match source.keystore_detail(&key_id).await {
                Ok(info) => {
                    // Keystore metadata line (D2): type, status and age.
                    let meta = format_key_meta(&info);
                    let pubk = info
                        .public_key
                        .unwrap_or_else(|| "(not a key pair)".to_string());
                    self.info_modal = Some(InfoModal {
                        title: "Key detail".into(),
                        body: format!(
                            "Key ID: {key_id}\n{meta}\nPublic key (shareable):\n\n{pubk}\n\n\
                             (The private key is NEVER shown.)"
                        ),
                    });
                }
                Err(e) => self.notify_error(format!("keystore: {}", e.code)),
            },
            PendingAction::KeystoreGenerate { key_id } => {
                match source.keystore_generate(&key_id).await {
                    Ok(pubk) => {
                        self.audit(format!("generated/rotated key '{key_id}'"));
                        self.info_modal = Some(InfoModal {
                            title: "Key generated".into(),
                            body: format!(
                                "Key ID: {key_id}\nNew public key:\n\n{pubk}\n\n\
                                 (The private key was stored; it is never shown.)"
                            ),
                        });
                        self.needs_fetch = true;
                    }
                    Err(e) => self.notify_error(format!("generate: {}", e.code)),
                }
            }
            PendingAction::KvExport { path } => {
                // Local dump of the already-loaded keys as a JSON object.
                let map: serde_json::Map<String, serde_json::Value> = self
                    .kv_entries
                    .iter()
                    .map(|e| {
                        (
                            e.key.clone(),
                            serde_json::Value::String(e.value_utf8.clone()),
                        )
                    })
                    .collect();
                match serde_json::to_string_pretty(&serde_json::Value::Object(map))
                    .map_err(|e| e.to_string())
                    .and_then(|json| std::fs::write(&path, json).map_err(|e| e.to_string()))
                {
                    Ok(()) => {
                        self.notify_success(format!("{} keys → {path}", self.kv_entries.len()))
                    }
                    Err(e) => self.notify_error(format!("export: {e}")),
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Rendering — layout and drawing of each screen
// ═══════════════════════════════════════════════════════════

fn ui(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Global background: paint the entire screen with the brand's dark tone (#181A1B)
    // before any widget. Cells not overwritten inherit this background, and the
    // following widgets (which don't set `bg`) preserve it.
    frame.render_widget(Block::default().style(Style::default().bg(APP_BG)), area);

    // Layout: Header (6, with logo) | Body (flex) | Footer (dynamic: rows of
    // button-cards + log). The footer height is capped to preserve the body.
    let footer_h = footer_desired_height(app, area.width)
        .min(area.height.saturating_sub(11))
        .max(3);
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),        // header (emblem + wordmark + node/up/peers)
            Constraint::Min(5),           // body
            Constraint::Length(footer_h), // footer
        ])
        .split(area);

    render_header(frame, main_layout[0], app);
    render_body(frame, main_layout[1], app);
    render_footer(frame, main_layout[2], app);

    // Overlays (on top of everything).
    if let Some(w) = &app.wizard {
        render_wizard(frame, area, w);
    }
    if let Some(w) = &app.store_wizard {
        render_store_wizard(frame, area, w);
    }
    if let Some(w) = &app.import_wizard {
        render_import_wizard(frame, area, w);
    }
    if let Some(m) = &app.info_modal {
        render_info_modal(frame, area, m);
    }
    if let Some(m) = &app.help_modal {
        render_help_modal(frame, area, m);
    }
}

/// Centered rectangle occupying `pct_x`% × `pct_y`% of the area.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// Persistent info box (e.g. a manifest hash for the user to copy).
fn render_info_modal(frame: &mut Frame, area: Rect, modal: &InfoModal) {
    let rect = centered_rect(70, 40, area);
    frame.render_widget(Clear, rect);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    for l in modal.body.lines() {
        lines.push(Line::from(Span::styled(
            l.to_string(),
            Style::default().fg(Color::White),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "Select the text with the mouse to copy. Press any key to close.",
        Style::default().fg(Color::DarkGray),
    )]));

    let inner = draw_window(frame, rect, &modal.title);
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

/// Contextual help (G4.1): large overlay with plain-language text. Light colors
/// per line type (section title, shortcut, body).
fn render_help_modal(frame: &mut Frame, area: Rect, modal: &InfoModal) {
    let rect = centered_rect(84, 84, area);
    frame.render_widget(Clear, rect);

    let mut lines: Vec<Line> = Vec::new();
    for l in modal.body.lines() {
        // Simple highlighting: "TITLE:" lines (no indent, ending in ':') in cyan;
        // lines starting with "  •" in gray; the rest white.
        let style = if l.ends_with(':') && !l.starts_with(' ') {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if l.trim_start().starts_with('•') {
            Style::default().fg(Color::Gray)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(l.to_string(), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "Press any key to close.",
        Style::default().fg(Color::DarkGray),
    )]));

    let inner = draw_window(frame, rect, &format!("❔ Help — {}", modal.title));
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

/// Multi-step controller creation wizard (feature 4.4).
fn render_wizard(frame: &mut Frame, area: Rect, w: &ControllerWizard) {
    let rect = centered_rect(70, 60, area);
    frame.render_widget(Clear, rect);

    // Summary of completed steps + active field.
    let field = |label: &str, value: &str, active: bool| -> Line {
        let vstyle = if active {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let shown = if active {
            format!("{value}\u{2588}")
        } else {
            value.to_string()
        };
        Line::from(vec![
            Span::styled(
                format!("  {label}: "),
                Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }),
            ),
            Span::styled(shown, vstyle),
        ])
    };

    let mut lines: Vec<Line> = vec![Line::from("")];

    // Type (list when active; value when already chosen).
    if w.step == WizardStep::Type {
        lines.push(Line::from(vec![Span::styled(
            "  Controller type (\u{2191}\u{2193} to choose, Enter confirms):",
            Style::default().fg(Color::Cyan),
        )]));
        for (i, t) in CTRL_TYPES.iter().enumerate() {
            let sel = i == w.type_idx;
            lines.push(Line::from(vec![Span::styled(
                format!("    {} {t}", if sel { "\u{25B6}" } else { " " }),
                if sel {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                },
            )]));
        }
    } else {
        lines.push(field("Type", w.controller_type(), false));
    }

    // Name.
    let name_val = if w.step == WizardStep::Name {
        &w.buffer
    } else {
        &w.name
    };
    if w.step as u8 >= WizardStep::Name as u8 {
        lines.push(field("Name", name_val, w.step == WizardStep::Name));
    }
    // Admin keys.
    let admin_val = if w.step == WizardStep::Admin {
        &w.buffer
    } else {
        &w.admin_keys
    };
    if w.step as u8 >= WizardStep::Admin as u8 {
        lines.push(field(
            "Admin (comma)",
            admin_val,
            w.step == WizardStep::Admin,
        ));
    }
    // Write keys.
    let write_val = if w.step == WizardStep::Write {
        &w.buffer
    } else {
        &w.write_keys
    };
    if w.step as u8 >= WizardStep::Write as u8 {
        lines.push(field(
            "Write (comma)",
            write_val,
            w.step == WizardStep::Write,
        ));
    }

    lines.push(Line::from(""));
    let hint = match w.step {
        WizardStep::Confirm => "Enter creates the controller · Esc back",
        _ => "Enter advances · Esc back/cancel",
    };
    lines.push(Line::from(vec![Span::styled(
        format!("  {hint}"),
        Style::default().fg(Color::DarkGray),
    )]));

    let inner = draw_window(frame, rect, "New Access Controller");
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

/// "New store" wizard (G1.4): Type → Name → Options → Confirm, with
/// plain-language descriptions for those unfamiliar with the concepts.
fn render_store_wizard(frame: &mut Frame, area: Rect, w: &StoreWizard) {
    let rect = centered_rect(72, 66, area);
    frame.render_widget(Clear, rect);
    let mut lines: Vec<Line> = vec![Line::from("")];

    // Type step — list with description.
    if w.step == StoreWizardStep::Kind {
        lines.push(Line::from(vec![Span::styled(
            "  Store type (\u{2191}\u{2193} chooses, Enter confirms):",
            Style::default().fg(Color::Cyan),
        )]));
        for (i, (name, desc)) in STORE_KINDS.iter().enumerate() {
            let sel = i == w.kind_idx;
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {} {name}  ", if sel { "\u{25B6}" } else { " " }),
                    if sel {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Span::styled(*desc, Style::default().fg(Color::DarkGray)),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Type: ", Style::default().fg(Color::DarkGray)),
            Span::styled(w.kind(), Style::default().fg(Color::Cyan)),
        ]));
    }

    // Name step.
    if w.step as u8 >= StoreWizardStep::Name as u8 {
        let active = w.step == StoreWizardStep::Name;
        let shown = if active {
            format!("{}\u{2588}", w.name)
        } else {
            w.name.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(
                "  Name: ",
                Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }),
            ),
            Span::styled(
                shown,
                if active {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
        ]));
    }

    // Options step — toggles with description.
    if w.step as u8 >= StoreWizardStep::Options as u8 {
        let active = w.step == StoreWizardStep::Options;
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            if active {
                "  Options (\u{2191}\u{2193} navigate, Space toggles, Enter advances):"
            } else {
                "  Options:"
            },
            Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }),
        )]));
        let toggles = [
            (
                0usize,
                "Replicate on the network",
                w.replicate,
                "shares/syncs with peers",
            ),
            (
                1,
                "Local only",
                w.local_only,
                "does not advertise or replicate",
            ),
            (
                2,
                "Read only",
                w.read_only,
                "replica that does not accept local writes",
            ),
        ];
        for (idx, label, on, desc) in toggles {
            let sel = active && w.opt_idx == idx;
            let mark = if on { "[x]" } else { "[ ]" };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "    {} {mark} {label}  ",
                        if sel { "\u{25B6}" } else { " " }
                    ),
                    if sel {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(if on { Color::Green } else { Color::Gray })
                    },
                ),
                Span::styled(desc, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    // ACL step (G2.3) — optional address of a controller to attach.
    if w.step as u8 >= StoreWizardStep::Acl as u8 {
        let active = w.step == StoreWizardStep::Acl;
        let shown = if active {
            format!("{}\u{2588}", w.acl)
        } else if w.acl.trim().is_empty() {
            "(none — default access)".to_string()
        } else {
            w.acl.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(
                "  ACL (optional): ",
                Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }),
            ),
            Span::styled(shown, Style::default().fg(Color::White)),
        ]));
        if active {
            lines.push(Line::from(vec![Span::styled(
                "    paste a controller's address (create one with F4); empty = default",
                Style::default().fg(Color::DarkGray),
            )]));
        }
    }

    lines.push(Line::from(""));
    let hint = match w.step {
        StoreWizardStep::Confirm => "Enter CREATES the store · Esc back",
        StoreWizardStep::Options => "Space toggles · Enter advances · Esc back",
        _ => "Enter advances · Esc back/cancel",
    };
    lines.push(Line::from(vec![Span::styled(
        format!("  {hint}"),
        Style::default().fg(Color::Yellow),
    )]));

    let inner = draw_window(frame, rect, "New Store");
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

/// "Import store" wizard (G3.3): Type → Name → paste the DocTicket.
fn render_import_wizard(frame: &mut Frame, area: Rect, w: &ImportWizard) {
    let rect = centered_rect(74, 60, area);
    frame.render_widget(Clear, rect);
    let mut lines: Vec<Line> = vec![Line::from("")];

    // Type.
    if w.step == ImportStep::Kind {
        lines.push(Line::from(vec![Span::styled(
            "  Shared store type (\u{2191}\u{2193}, Enter confirms):",
            Style::default().fg(Color::Cyan),
        )]));
        for (i, (name, desc)) in IMPORT_KINDS.iter().enumerate() {
            let sel = i == w.kind_idx;
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {} {name}  ", if sel { "\u{25B6}" } else { " " }),
                    if sel {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Span::styled(*desc, Style::default().fg(Color::DarkGray)),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Type: ", Style::default().fg(Color::DarkGray)),
            Span::styled(w.kind(), Style::default().fg(Color::Cyan)),
        ]));
    }

    // Local name.
    if w.step as u8 >= ImportStep::Name as u8 {
        let active = w.step == ImportStep::Name;
        let shown = if active {
            format!("{}\u{2588}", w.name)
        } else {
            w.name.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(
                "  Local name: ",
                Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }),
            ),
            Span::styled(shown, Style::default().fg(Color::White)),
        ]));
    }

    // Ticket + read-only.
    if w.step == ImportStep::Ticket {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Paste the received DocTicket (Tab toggles read-only):",
            Style::default().fg(Color::Cyan),
        )]));
        lines.push(Line::from(vec![Span::styled(
            format!("    {}\u{2588}", preview(&w.ticket, 60)),
            Style::default().fg(Color::White),
        )]));
        lines.push(Line::from(vec![
            Span::styled("    Read only: ", Style::default().fg(Color::Gray)),
            Span::styled(
                if w.read_only { "[x] yes" } else { "[ ] no" },
                Style::default().fg(if w.read_only {
                    Color::Green
                } else {
                    Color::Gray
                }),
            ),
        ]));
    }

    lines.push(Line::from(""));
    let hint = match w.step {
        ImportStep::Ticket => "Enter IMPORTS · Tab toggles read-only · Esc back",
        _ => "Enter advances · Esc back/cancel",
    };
    lines.push(Line::from(vec![Span::styled(
        format!("  {hint}"),
        Style::default().fg(Color::Yellow),
    )]));

    let inner = draw_window(frame, rect, "Import Store");
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

/// Brand header: emblem (gear + helm) on the left, "GUARDIAN-DB" wordmark +
/// node data (full NodeId, uptime, peers, source) on the right, and a gold rule
/// at the bottom. Replaces the old single line.
fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    // Content occupies all lines but the last (the rule).
    let content = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(11), Constraint::Min(20)])
        .split(content);

    // Header emblem: crisp image via graphics protocol when available;
    // otherwise quadrant blocks; with no image, ASCII art.
    let drew_header = app.graphics && {
        let mut guard = app.header_proto.borrow_mut();
        match guard.as_mut() {
            Some(proto) => {
                draw_protocol_centered(frame, cols[0], proto);
                true
            }
            None => false,
        }
    };
    if drew_header {
        // drawn via protocol
    } else if let Some(img) = &app.header_img {
        draw_image_blocks(frame, cols[0], img);
    } else {
        let emblem = ["▟██████▙", "██▐▟▙▌██", "██▐▜▛▌██", "▜██████▛"];
        let emblem_lines: Vec<Line> = emblem
            .iter()
            .enumerate()
            .map(|(i, r)| {
                Line::from(Span::styled(
                    *r,
                    Style::default()
                        .fg(GOLD_GRADIENT[i.min(4)])
                        .add_modifier(Modifier::BOLD),
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(emblem_lines), cols[0]);
    }

    // Info column.
    let gray = Style::default().fg(Color::Gray);
    let node = if app.node_id.is_empty() {
        "connecting…".to_string()
    } else {
        app.node_id.clone()
    };
    let peers_color = if app.peers_online > 0 {
        Color::Green
    } else {
        Color::DarkGray
    };
    let info = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "GUARDIAN",
                Style::default().fg(BRAND_GOLD).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "-DB",
                Style::default()
                    .fg(GOLD_GRADIENT[3])
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   Administration Panel", Style::default().fg(Color::Gray)),
        ]),
        Line::from(vec![
            Span::styled("Node: ", gray),
            Span::styled(node, Style::default().fg(BRAND_GOLD)),
        ]),
        Line::from(vec![
            Span::styled("Up: ", gray),
            Span::styled(app.uptime(), Style::default().fg(Color::Green)),
            Span::styled("    Peers: ", gray),
            Span::styled(
                app.peers_online.to_string(),
                Style::default().fg(peers_color),
            ),
            Span::styled("    Source: ", gray),
            Span::styled(
                app.source_label.clone(),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(info), cols[1]);

    // Version in the top-right corner.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "SENTINEL 1.0.0 ",
            Style::default().fg(BRAND_GOLD).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Right),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );

    // Gold rule on the header's last line.
    let y = area.y + area.height - 1;
    let bs = Style::default().fg(BRAND_GOLD_DIM);
    let buf = frame.buffer_mut();
    for x in area.x..(area.x + area.width) {
        if let Some(c) = buf.cell_mut((x, y)) {
            c.set_symbol("─").set_style(bs);
        }
    }
}

fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    match &app.screen {
        Screen::Connecting => render_connecting(frame, area, app),
        Screen::Dashboard => render_dashboard(frame, area, app),
        Screen::ConnectionFailed { message } => render_connection_error(frame, area, message),
        Screen::KeyValueInspector { kv_name } => render_kv_inspector(frame, area, app, kv_name),
        Screen::DocumentInspector { store_name } => {
            render_document_inspector(frame, area, app, store_name)
        }
        Screen::EventLogInspector { log_name } => {
            render_eventlog_inspector(frame, area, app, log_name)
        }
        Screen::ReplicationMonitor => render_replication_monitor(frame, area, app),
        Screen::BlobBrowser => render_blob_browser(frame, area, app),
        Screen::AccessControlManager => render_acl_manager(frame, area, app),
        Screen::KeystoreManager => render_keystore_manager(frame, area, app),
        Screen::NetworkTopology => render_network_topology(frame, area, app),
        Screen::EventBusExplorer => render_eventbus(frame, area, app),
        Screen::StoreDetail { store_address } => {
            render_store_detail(frame, area, app, store_address)
        }
        // Screens not yet implemented — placeholder
        _ => render_placeholder(frame, area, &app.screen),
    }
}

/// Detail of a store (feature 1.3): metadata from the already-loaded `StoreInfo`.
/// Used mainly for `document` stores (KV/EventLog have their own inspectors).
fn render_store_detail(frame: &mut Frame, area: Rect, app: &App, address: &str) {
    let store = app.stores.iter().find(|s| s.address == address);
    let mut lines = vec![Line::from("")];
    match store {
        Some(s) => {
            let field = |label: &str, value: String, color: Color| {
                Line::from(vec![
                    Span::styled(format!("  {label}: "), Style::default().fg(Color::Gray)),
                    Span::styled(value, Style::default().fg(color)),
                ])
            };
            lines.push(field("Name", s.db_name.clone(), Color::White));
            lines.push(field("Type", s.store_type.clone(), Color::Cyan));
            lines.push(field("Address", s.address.clone(), Color::DarkGray));
            lines.push(field("Entries", s.entry_count.to_string(), Color::White));
            lines.push(Line::from(vec![
                Span::styled("  Status: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{} {}", s.sync_status.icon(), s.sync_status.label()),
                    Style::default().fg(s.sync_status.color()),
                ),
            ]));
            // Peers observed syncing this store (B2, derived from the events).
            lines.push(Line::from(""));
            let peers = app.peers_for_store(&s.address);
            if peers.is_empty() {
                lines.push(Line::from(vec![Span::styled(
                    "  Peers (observed): no sync seen since the panel opened",
                    Style::default().fg(Color::DarkGray),
                )]));
            } else {
                lines.push(Line::from(vec![Span::styled(
                    format!("  Peers observed syncing ({}):", peers.len()),
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                )]));
                for (peer, syncs, last) in peers.iter().take(8) {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("    {} ", preview(peer, 20)),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            format!("· {syncs} syncs · {last}s ago"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "  'c' connects a peer to this store · Esc back",
                Style::default().fg(Color::DarkGray),
            )]));
        }
        None => lines.push(Line::from(vec![Span::styled(
            "  Store not found (was it closed?).",
            Style::default().fg(Color::DarkGray),
        )])),
    }

    let inner = draw_gold_panel(frame, area, "Store Detail");
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

/// Color + icon per access controller type (feature 4.1). Note: today the core
/// maps all types to `SimpleAccessController`, so in practice most appear as
/// `simple` — the color differentiates when the manifest records another type.
fn controller_type_style(t: &str) -> (Color, &'static str) {
    match t {
        "simple" => (Color::Blue, "🔵"),
        "guardian" => (Color::Green, "🟢"),
        "iroh" => (Color::Magenta, "🟣"),
        _ => (Color::Gray, "⚪"),
    }
}

/// Roles toggleable in the ACL grant/revoke selector (feature 4.3). Tab cycles.
const ACL_ROLES: [&str; 2] = ["write", "admin"];

/// Next role in the cycle (write ↔ admin).
fn next_acl_role(role: &str) -> String {
    let i = ACL_ROLES.iter().position(|r| *r == role).unwrap_or(0);
    ACL_ROLES[(i + 1) % ACL_ROLES.len()].to_string()
}

/// Label of the grant/revoke prompt, showing the active role and the Tab hint.
fn acl_role_label(grant: bool, role: &str) -> String {
    let verb = if grant { "grant" } else { "revoke" };
    format!("Key ID to {verb} '{role}' (Tab switches role): ")
}

/// Color per event kind (for quick reading in the explorer).
fn event_kind_color(kind: &str) -> Color {
    match kind {
        "sync" | "sync_completed" => Color::Green,
        "peer_connected" => Color::Cyan,
        "peer_disconnected" => Color::Yellow,
        "store_updated" => Color::Blue,
        "sync_error" => Color::Red,
        "database_created" => Color::Magenta,
        _ => Color::Gray,
    }
}

/// EventBus Explorer (feature 8): live event stream with statistics,
/// kind filter, search, follow and pause.
fn render_eventbus(frame: &mut Frame, area: Rect, app: &App) {
    // Layout: statistics (5) | activity sparkline (3) | event list (flex).
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .split(area);

    // ── Statistics (8.3): total, rate, count per kind, mode ──
    let counts = app.event_counts();
    let mut count_spans: Vec<Span> = Vec::new();
    for (k, n) in counts.iter() {
        count_spans.push(Span::styled(
            format!(" {k}:"),
            Style::default().fg(event_kind_color(k)),
        ));
        count_spans.push(Span::styled(
            format!("{n}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if count_spans.is_empty() {
        count_spans.push(Span::styled(
            " (no events yet)",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let filter = EVENT_KINDS[app.event_kind_filter];
    let mode_line = Line::from(vec![
        Span::styled(" Total: ", Style::default().fg(Color::Gray)),
        Span::styled(
            app.events.len().to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   Rate: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{:.1}/s", app.events_per_sec()),
            Style::default().fg(Color::Green),
        ),
        Span::styled("   Filter: ", Style::default().fg(Color::Gray)),
        Span::styled(filter, Style::default().fg(Color::Cyan)),
        Span::styled("   Follow: ", Style::default().fg(Color::Gray)),
        Span::styled(
            if app.event_follow { "on" } else { "off" },
            Style::default().fg(if app.event_follow {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled("   ", Style::default()),
        Span::styled(
            if app.event_paused { "⏸ PAUSED" } else { "" },
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    // Top peers by event volume (feature 8.3 / A1, via B1).
    let top = app.top_peers(3);
    let mut top_spans: Vec<Span> = vec![Span::styled(
        " Top peers: ",
        Style::default().fg(Color::Gray),
    )];
    if top.is_empty() {
        top_spans.push(Span::styled(
            "(no events with a peer)",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        for (i, (peer, n)) in top.iter().enumerate() {
            if i > 0 {
                top_spans.push(Span::styled("  ", Style::default()));
            }
            top_spans.push(Span::styled(
                preview(peer, 16),
                Style::default().fg(Color::Cyan),
            ));
            top_spans.push(Span::styled(
                format!(" ({n})"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }

    let stats_inner = draw_gold_panel(frame, layout[0], "Statistics");
    let stats = Paragraph::new(vec![
        mode_line,
        Line::from(count_spans),
        Line::from(top_spans),
    ]);
    frame.render_widget(stats, stats_inner);

    // ── EventBus activity sparkline, last 60s (feature 8.3) ──
    let spark_data = app.event_sparkline();
    let spark_max = spark_data.iter().copied().max().unwrap_or(0);
    let spark_inner = draw_gold_panel(
        frame,
        layout[1],
        &format!("Activity 60s (peak {spark_max}/s)"),
    );
    let sparkline = Sparkline::default()
        .data(&spark_data)
        .style(Style::default().fg(Color::Green));
    frame.render_widget(sparkline, spark_inner);

    // ── Event list (8.1) with search highlight ──
    let query = app.search.as_deref().unwrap_or("");
    let base = Style::default().fg(Color::Gray);
    let hl = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let visible = app.visible_events();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|e| {
            let mut spans = vec![
                Span::styled(format!(" {} ", e.ts), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:<17} ", preview(&e.kind, 17)),
                    Style::default()
                        .fg(event_kind_color(&e.kind))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            ];
            spans.extend(highlight_spans(&preview(&e.detail, 50), query, base, hl));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let total = app.events.len();
    let title = if visible.len() == total {
        format!(" Events ({total}) ")
    } else {
        format!(" Events ({} of {}) ", visible.len(), total)
    };
    render_inspector_list(
        frame,
        layout[2],
        title,
        items,
        "No events (waiting for EventBus activity).",
        &app.inspector_state,
    );
}

/// Color by latency quality (feature 6.2): green <50ms, yellow <200ms, red above.
fn latency_color(ms: f64) -> Color {
    if ms < 50.0 {
        Color::Green
    } else if ms < 200.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Network topology viewer (feature 6): ASCII star graph from this node, with
/// link type (solid=direct/mDNS, dashed=relay), latency colored by quality and
/// operation count.
fn render_network_topology(frame: &mut Frame, area: Rect, app: &App) {
    // Layout: graph root (this node, 3) | link list (flex). The root sits OUTSIDE
    // the list so the selection (inspector_state, indexed by app.topo.len())
    // aligns exactly with the links.
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Min(3),
        ])
        .split(area);

    let node = if app.node_id.is_empty() {
        "this-node".to_string()
    } else {
        preview(&app.node_id, 16)
    };
    let root_inner = draw_gold_panel(frame, layout[0], "");
    let root = Paragraph::new(Line::from(vec![Span::styled(
        format!(" ◆ {node}  (this node)"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    frame.render_widget(root, root_inner);

    // ── Relay status (C2) + global latency percentiles (C1) ──
    let relay_line = if app.relays.is_empty() {
        Line::from(vec![Span::styled(
            " Relay: (no home-relay configured/selected)",
            Style::default().fg(Color::DarkGray),
        )])
    } else {
        let mut spans = vec![Span::styled(" Relay: ", Style::default().fg(Color::Gray))];
        for (i, r) in app.relays.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ", Style::default()));
            }
            let (icon, color) = if r.connected {
                ("●", Color::Green)
            } else {
                ("○", Color::Red)
            };
            spans.push(Span::styled(format!("{icon} "), Style::default().fg(color)));
            spans.push(Span::styled(
                preview(&r.url, 32),
                Style::default().fg(Color::White),
            ));
            if let Some(err) = &r.last_error {
                spans.push(Span::styled(
                    format!(" (error: {})", preview(err, 20)),
                    Style::default().fg(Color::Red),
                ));
            }
        }
        Line::from(spans)
    };
    let lat_line = match app.latency {
        Some(l) => Line::from(vec![
            Span::styled(" Global latency: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("p95 {:.0}ms", l.p95_ms),
                Style::default().fg(latency_color(l.p95_ms)),
            ),
            Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("p99 {:.0}ms", l.p99_ms),
                Style::default().fg(latency_color(l.p99_ms)),
            ),
            Span::styled(
                "   (node-wide, not per-peer)",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        None => Line::from(vec![Span::styled(
            " Global latency: (no samples yet)",
            Style::default().fg(Color::DarkGray),
        )]),
    };
    // Aggregate throughput (D1).
    let thr_line = match app.throughput {
        Some(t) => Line::from(vec![
            Span::styled(" Throughput: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}/s", human_bytes(t.bytes_per_second)),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1} ops/s", t.ops_per_second),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!(
                    "   (peak {:.1}, avg {:.1})",
                    t.peak_throughput, t.avg_throughput
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        None => Line::from(vec![Span::styled(
            " Throughput: (no data)",
            Style::default().fg(Color::DarkGray),
        )]),
    };
    // Known but not connected peers (C3).
    let disc_line = if app.discovered.is_empty() {
        Line::from(vec![Span::styled(
            " Known offline: none",
            Style::default().fg(Color::DarkGray),
        )])
    } else {
        let list = app
            .discovered
            .iter()
            .map(|p| preview(p, 12))
            .collect::<Vec<_>>()
            .join(", ");
        Line::from(vec![
            Span::styled(
                format!(" Known offline ({}): ", app.discovered.len()),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(preview(&list, 56), Style::default().fg(Color::Yellow)),
        ])
    };
    let net_inner = draw_gold_panel(
        frame,
        layout[1],
        "Network (relay · latency · throughput · discovery)",
    );
    let net_info = Paragraph::new(vec![relay_line, lat_line, thr_line, disc_line]);
    frame.render_widget(net_info, net_inner);

    // List = just the links (├──/└── edge + metrics).
    let last = app.topo.len().saturating_sub(1);
    let items: Vec<ListItem> = app
        .topo
        .iter()
        .enumerate()
        .map(|(i, l)| {
            // Uses the REAL conn-type (C1) when available; otherwise the one inferred from the address.
            let kind = l.conn_type.as_deref().unwrap_or(&l.link_kind);
            let real = l.conn_type.is_some();
            let dashed = kind == "relay";
            let branch = if i == last { "└" } else { "├" };
            let edge = if dashed { "╌╌╌" } else { "───" };
            let lat = latency_color(l.latency_ms);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {branch}{edge} "), Style::default().fg(lat)),
                Span::styled("● ", Style::default().fg(lat)),
                Span::styled(
                    format!("{} ", preview(&l.node_id, 12)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    // '~' suffix marks an inferred type (not confirmed by remote_info).
                    format!("[{}{}] ", kind, if real { "" } else { "~" }),
                    Style::default().fg(if dashed { Color::Magenta } else { Color::Green }),
                ),
                Span::styled(
                    format!("{:.0}ms ", l.latency_ms),
                    Style::default().fg(lat).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("· {} ops · {}s ", l.ops, l.connected_secs),
                    Style::default().fg(Color::DarkGray),
                ),
                // Per-peer p95/p99 (C1) when there are enough samples.
                match (l.p95_ms, l.p99_ms) {
                    (Some(p95), Some(p99)) => Span::styled(
                        format!("[p95 {p95:.0} p99 {p99:.0}] "),
                        Style::default().fg(Color::Magenta),
                    ),
                    _ => Span::styled("", Style::default()),
                },
                Span::styled(
                    preview(&l.address, 20),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    // Count by the effective type (real when available, otherwise inferred).
    let kind_of = |l: &TopoLink| l.conn_type.clone().unwrap_or_else(|| l.link_kind.clone());
    let direct = app.topo.iter().filter(|l| kind_of(l) == "direct").count();
    let relay = app.topo.iter().filter(|l| kind_of(l) == "relay").count();
    let title = format!(
        " Links ({}): {direct} direct, {relay} relay ",
        app.topo.len()
    );
    render_inspector_list(
        frame,
        layout[2],
        title,
        items,
        "No active connections.",
        &app.inspector_state,
    );
}

/// Sanitizes a value for a single line (strips control characters) and truncates
/// at `max` characters, with an ellipsis.
fn preview(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() > max {
        let head: String = cleaned.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        cleaned
    }
}

/// Common helper for the inspection screens: a scrollable list, or a message if empty.
fn render_inspector_list(
    frame: &mut Frame,
    area: Rect,
    title: String,
    items: Vec<ListItem>,
    empty_msg: &str,
    state: &ListState,
) {
    let inner = draw_gold_panel(frame, area, &title);

    if items.is_empty() {
        let p = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                format!("  {empty_msg}"),
                Style::default().fg(Color::DarkGray),
            )]),
        ]);
        frame.render_widget(p, inner);
    } else {
        let list = List::new(items)
            // Selection highlighted in gold (BIOS style: amber bar).
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(BRAND_GOLD)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, inner, &mut state.clone());
    }
}

fn render_kv_inspector(frame: &mut Frame, area: Rect, app: &App, name: &str) {
    let query = app.search.as_deref().unwrap_or("");
    let base = Style::default().fg(Color::White);
    let hl = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let kbase = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);

    let visible = app.visible_kv_entries();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|e| {
            let mut spans = vec![Span::styled(" ", Style::default())];
            spans.extend(highlight_spans(
                &format!("{:<24}", preview(&e.key, 24)),
                query,
                kbase,
                hl,
            ));
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            spans.extend(highlight_spans(
                &preview(&e.value_utf8, 48),
                query,
                base,
                hl,
            ));
            spans.push(Span::styled(
                format!("  ({} b)", e.size),
                Style::default().fg(Color::DarkGray),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = match &app.search {
        Some(q) if !q.is_empty() => format!(
            " KeyValue: {} ({} of {}) ",
            preview(name, 32),
            visible.len(),
            app.kv_entries.len()
        ),
        _ => format!(
            " KeyValue: {} ({}) ",
            preview(name, 40),
            app.kv_entries.len()
        ),
    };
    let empty_msg = if app.search.as_deref().unwrap_or("").is_empty() {
        "No keys — press 'n' to create the first one."
    } else {
        "No key matches the search."
    };
    render_inspector_list(frame, area, title, items, empty_msg, &app.inspector_state);
}

/// Document store inspector (B4): document id + JSON preview, analogous to the
/// KV inspector. `Enter` opens the full document (via the `docs.get` op).
fn render_document_inspector(frame: &mut Frame, area: Rect, app: &App, name: &str) {
    let query = app.search.as_deref().unwrap_or("");
    let base = Style::default().fg(Color::White);
    let hl = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let kbase = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);

    let visible = app.visible_doc_entries();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|d| {
            let mut spans = vec![Span::styled(" ", Style::default())];
            spans.extend(highlight_spans(
                &format!("{:<24}", preview(&d.id, 24)),
                query,
                kbase,
                hl,
            ));
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            spans.extend(highlight_spans(
                &preview(&d.value_utf8, 48),
                query,
                base,
                hl,
            ));
            spans.push(Span::styled(
                format!("  ({} b)", d.size),
                Style::default().fg(Color::DarkGray),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = match &app.search {
        Some(q) if !q.is_empty() => format!(
            " Document: {} ({} of {}) ",
            preview(name, 32),
            visible.len(),
            app.doc_entries.len()
        ),
        _ => format!(
            " Document: {} ({}) ",
            preview(name, 40),
            app.doc_entries.len()
        ),
    };
    let empty_msg = if app.search.as_deref().unwrap_or("").is_empty() {
        "No documents — press 'n' to create the first one."
    } else {
        "No document matches the search."
    };
    render_inspector_list(frame, area, title, items, empty_msg, &app.inspector_state);
}

fn render_eventlog_inspector(frame: &mut Frame, area: Rect, app: &App, name: &str) {
    let query = app.search.as_deref().unwrap_or("");
    let base = Style::default().fg(Color::White);
    let hl = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let visible = app.visible_log_entries();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|e| {
            let key = e.key.clone().unwrap_or_default();
            let mut spans = vec![
                Span::styled(
                    format!(" {:>4} ", e.index),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<4} ", preview(&e.op, 4)),
                    Style::default().fg(Color::Blue),
                ),
                if key.is_empty() {
                    Span::styled("", Style::default())
                } else {
                    Span::styled(
                        format!("{} ", preview(&key, 16)),
                        Style::default().fg(Color::Cyan),
                    )
                },
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            ];
            // Payload preview with the searched terms highlighted.
            spans.extend(highlight_spans(
                &preview(&e.value_utf8, 44),
                query,
                base,
                hl,
            ));
            spans.push(Span::styled(
                format!("  ({} b)", e.size),
                Style::default().fg(Color::DarkGray),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();

    // Active logical clock range badge (B5), e.g. " clock 5-20".
    let clock_badge = match app.log_clock_range {
        Some((min, max)) => {
            let lo = min.map(|v| v.to_string()).unwrap_or_default();
            let hi = max.map(|v| v.to_string()).unwrap_or_default();
            format!(" clock {lo}-{hi}")
        }
        None => String::new(),
    };
    // Title with an "N of M" counter when any filter is active (search or clock).
    let has_filter =
        app.search.as_deref().is_some_and(|q| !q.is_empty()) || app.log_clock_range.is_some();
    let title = if has_filter {
        format!(
            " EventLog: {} ({} of {}{}) ",
            preview(name, 28),
            visible.len(),
            app.log_entries.len(),
            clock_badge,
        )
    } else {
        format!(
            " EventLog: {} ({}) ",
            preview(name, 40),
            app.log_entries.len()
        )
    };
    let empty_msg = if !has_filter {
        "No entries — press 'a' to append the first one."
    } else {
        "No entry matches the filters."
    };
    render_inspector_list(frame, area, title, items, empty_msg, &app.inspector_state);
}

fn render_replication_monitor(frame: &mut Frame, area: Rect, app: &App) {
    // Layout: sync dashboard (5) | sparkline (3) | peer list (flex).
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .split(area);

    // ── Real-time sync dashboard (5.3) + diagnostics (5.4) ──
    let online = app.peers.iter().filter(|p| p.connected).count();
    let offline = app.peers.len() - online;
    let syncs_min = app.syncs_last_min();
    let errors = app.sync_errors_last_min();
    // Sync duration (5.3, via B1) and approximation of syncs in progress.
    let dur_stats = app.sync_duration_stats();
    let in_progress = app.syncs_in_progress();
    // Connected peers with no recent sync (> 5min) or never seen in the buffer.
    let stale = app
        .peers
        .iter()
        .filter(|p| p.connected && app.peer_last_sync_secs(&p.node_id).is_none_or(|s| s > 300))
        .count();

    let metrics = Line::from(vec![
        Span::styled(" Peers: ", Style::default().fg(Color::Gray)),
        Span::styled(
            app.peers.len().to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" (", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{online} online"),
            Style::default().fg(Color::Green),
        ),
        Span::styled(", ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{offline} offline"),
            Style::default().fg(if offline > 0 {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(")   │   ", Style::default().fg(Color::DarkGray)),
        Span::styled("Syncs/min: ", Style::default().fg(Color::Gray)),
        Span::styled(syncs_min.to_string(), Style::default().fg(Color::Green)),
        Span::styled("   │   ", Style::default().fg(Color::DarkGray)),
        Span::styled("Errors: ", Style::default().fg(Color::Gray)),
        Span::styled(
            errors.to_string(),
            Style::default().fg(if errors > 0 {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ),
    ]);

    // Sync line (5.3): average/last duration + syncs in progress (via B1).
    let sync_line = Line::from(vec![
        Span::styled(" Sync duration: ", Style::default().fg(Color::Gray)),
        match dur_stats {
            Some((avg, last)) => Span::styled(
                format!("last {last}ms · avg {avg}ms"),
                Style::default().fg(Color::Cyan),
            ),
            None => Span::styled(
                "(no completed sync in the buffer)",
                Style::default().fg(Color::DarkGray),
            ),
        },
        Span::styled("   │   ", Style::default().fg(Color::DarkGray)),
        Span::styled("In progress: ", Style::default().fg(Color::Gray)),
        Span::styled(
            in_progress.to_string(),
            Style::default().fg(if in_progress > 0 {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(
            "  (heads exchanged without completion, approximate)",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    // Diagnostics line (5.4).
    let diag = if errors > 0 {
        Line::from(vec![Span::styled(
            format!(" ⚠ {errors} sync error(s) in the last minute — check connectivity/ACL"),
            Style::default().fg(Color::Red),
        )])
    } else if stale > 0 {
        Line::from(vec![Span::styled(
            format!(" ⚠ {stale} connected peer(s) with no sync for >5min"),
            Style::default().fg(Color::Yellow),
        )])
    } else if app.peers.is_empty() {
        Line::from(vec![Span::styled(
            " No peers — this node is isolated or waiting for discovery.",
            Style::default().fg(Color::DarkGray),
        )])
    } else {
        Line::from(vec![Span::styled(
            " ✓ No problems detected.",
            Style::default().fg(Color::Green),
        )])
    };

    let dash_inner = draw_gold_panel(frame, layout[0], "Sync Dashboard");
    let dash = Paragraph::new(vec![metrics, sync_line, diag]);
    frame.render_widget(dash, dash_inner);

    // ── Sync activity sparkline (last 60s) ──
    let spark_data = app.sync_sparkline();
    let spark_inner = draw_gold_panel(frame, layout[1], "Sync Activity (60s)");
    let sparkline = Sparkline::default()
        .data(&spark_data)
        .style(Style::default().fg(Color::Green));
    frame.render_widget(sparkline, spark_inner);

    // ── Peer list (5.1) with problem highlighting (5.4) ──
    let items: Vec<ListItem> = app
        .peers
        .iter()
        .map(|p| {
            let last_sync = app.peer_last_sync_secs(&p.node_id);
            let stale_peer = p.connected && last_sync.is_none_or(|s| s > 300);
            let (icon, color) = if !p.connected {
                ("○", Color::DarkGray)
            } else if stale_peer {
                ("◐", Color::Yellow)
            } else {
                ("●", Color::Green)
            };
            let addr = p.addresses.first().cloned().unwrap_or_default();
            let sync_label = match last_sync {
                Some(s) if s < 60 => format!("sync {s}s ago"),
                Some(s) => format!("sync {}min ago", s / 60),
                None => "no sync".to_string(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {icon} "), Style::default().fg(color)),
                Span::styled(
                    format!("{} ", preview(&p.node_id, 20)),
                    Style::default().fg(Color::White),
                ),
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    if p.connected { "online  " } else { "offline " },
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!("{sync_label}  "),
                    Style::default().fg(if stale_peer {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(preview(&addr, 28), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let title = format!(" Peers ({}) ", app.peers.len());
    render_inspector_list(
        frame,
        layout[2],
        title,
        items,
        "No known peers.",
        &app.inspector_state,
    );
}

fn render_blob_browser(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .blobs
        .iter()
        .map(|b| {
            // Completeness indicator (C5): ● complete (green) · ◐ partial (yellow).
            let (icon, icon_color) = if b.complete {
                ("●", Color::Green)
            } else {
                ("◐", Color::Yellow)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {icon} "), Style::default().fg(icon_color)),
                Span::styled(
                    format!("{} ", preview(&b.hash, 46)),
                    Style::default().fg(Color::Magenta),
                ),
                Span::styled(
                    format!("  {}", human_bytes(b.size)),
                    Style::default().fg(Color::DarkGray),
                ),
                if b.complete {
                    Span::styled("", Style::default())
                } else {
                    Span::styled(" (partial)", Style::default().fg(Color::Yellow))
                },
            ]))
        })
        .collect();
    // Title with count, total disk usage (9.3) and sort criterion (9.1).
    let title = format!(
        " Blobs ({}) · {} on disk · sort: {} ",
        app.blobs.len(),
        human_bytes(app.blobs_total_size()),
        app.blob_sort.label(),
    );
    render_inspector_list(
        frame,
        area,
        title,
        items,
        "No blobs — press 'a' to add a file.",
        &app.inspector_state,
    );
}

/// Error screen shown when GuardianDB could not be opened — typically because
/// the `data-dir` is already under redb's exclusive lock (another process opened it).
fn render_connection_error(frame: &mut Frame, area: Rect, message: &str) {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "✗ Could not open GuardianDB",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
    ];
    for l in message.lines() {
        lines.push(Line::from(Span::styled(
            l.to_string(),
            Style::default().fg(Color::Gray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "Press 'q' to quit.",
        Style::default().fg(Color::Yellow),
    )]));

    let paragraph = Paragraph::new(lines)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(" Connection Error "),
        );

    frame.render_widget(paragraph, area);
}

// ── Guardian Sentinel brand: coral/terracotta palette (#D97757) + block banner ──
// (The `BRAND_GOLD*` / `GOLD_GRADIENT` names are kept because they're already used
//  throughout the file; the VALUES are now the brand coral, applied across the TUI.)

/// Brand coral/terracotta tone (#D97757) — consistent accent in the header and splash.
const BRAND_GOLD: Color = Color::Rgb(0xD9, 0x77, 0x57);

/// Global panel background (#181A1B) — painted over the whole screen in `ui()`.
const APP_BG: Color = Color::Rgb(0x18, 0x1A, 0x1B);

/// Coral gradient (light at top → deep terracotta at bottom), derived from #D97757.
const GOLD_GRADIENT: [Color; 5] = [
    Color::Rgb(0xEC, 0xA9, 0x8D), // light coral
    Color::Rgb(0xE3, 0x8E, 0x6F),
    Color::Rgb(0xD9, 0x77, 0x57), // base coral (#D97757)
    Color::Rgb(0xC2, 0x5E, 0x3F),
    Color::Rgb(0xA8, 0x4A, 0x2F), // deep terracotta
];

/// Darker coral tone, for panel borders (less glaring).
const BRAND_GOLD_DIM: Color = Color::Rgb(0x9E, 0x52, 0x38);

/// Draws a **square-cornered** panel with a coral border and a title on the top
/// border; returns the inner area for the content.
fn draw_gold_panel(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain) // square corners ┌ ┐ └ ┘
        .border_style(Style::default().fg(BRAND_GOLD_DIM));
    let t = title.trim();
    if !t.is_empty() {
        block = block.title(Span::styled(
            format!(" {t} "),
            Style::default().fg(BRAND_GOLD).add_modifier(Modifier::BOLD),
        ));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Background of the header band of windows/panels (title bar) — faint red.
const HEADER_BG: Color = Color::Rgb(0x4A, 0x24, 0x22);

/// Square-cornered window (modal/wizard) with a header + close button `Esc ✕`.
fn draw_window(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    draw_window_impl(frame, area, title, true)
}

/// Square-cornered panel with a **header** (title band + separator) but **no**
/// close button — for fixed on-screen panels (Metrics, Stores).
fn draw_headed_panel(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    draw_window_impl(frame, area, title, false)
}

/// Draws a square-cornered window/panel with its own **header** (title bar,
/// Windows-window style): a band at the top with the title on the left and, when
/// `closeable`, the close button `Esc ✕` on the right; followed by a `├───┤`
/// separator line. Returns the inner area **below the header** for the content.
fn draw_window_impl(frame: &mut Frame, area: Rect, title: &str, closeable: bool) -> Rect {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(BRAND_GOLD_DIM));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 6 || inner.height < 3 {
        return inner;
    }

    // ── Header band (1st inner line) ──
    let title_style = Style::default()
        .fg(BRAND_GOLD)
        .bg(HEADER_BG)
        .add_modifier(Modifier::BOLD);
    let band = Style::default().bg(HEADER_BG);
    let title_txt = format!(" {}", title.trim());
    let title_w = title_txt.chars().count() as u16;
    // Close button on the right (windows only): " Esc " (chip) + " ✕ ".
    let close_w: u16 = if closeable { 5 + 3 } else { 0 };
    let pad = inner.width.saturating_sub(title_w + close_w);
    let mut spans = vec![Span::styled(title_txt, title_style)];
    // Fill the rest of the band with the background (so the bar spans the whole line).
    spans.push(Span::styled(" ".repeat(pad as usize), band));
    if closeable && inner.width >= title_w + close_w {
        spans.push(Span::styled(
            " Esc ",
            Style::default()
                .fg(Color::Black)
                .bg(BRAND_GOLD)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            " ✕ ",
            Style::default().fg(BRAND_GOLD).bg(HEADER_BG),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    // ── Separator line (2nd inner line), connected to the box sides ──
    let sep_y = inner.y + 1;
    let bs = Style::default().fg(BRAND_GOLD_DIM);
    let buf = frame.buffer_mut();
    if let Some(c) = buf.cell_mut((area.x, sep_y)) {
        c.set_symbol("├").set_style(bs);
    }
    for x in inner.x..(inner.x + inner.width) {
        if let Some(c) = buf.cell_mut((x, sep_y)) {
            c.set_symbol("─").set_style(bs);
        }
    }
    if let Some(c) = buf.cell_mut((area.x + area.width - 1, sep_y)) {
        c.set_symbol("┤").set_style(bs);
    }

    // Content starts below the header + separator.
    Rect {
        x: inner.x,
        y: inner.y + 2,
        width: inner.width,
        height: inner.height.saturating_sub(2),
    }
}

/// Draws a square-cornered metric card: **centered title** on top and the **large
/// number** (colored, bold) centered below. No icon.
fn draw_metric_card(frame: &mut Frame, area: Rect, title: &str, value: &str, value_color: Color) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(BRAND_GOLD_DIM));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 2 || inner.height < 1 {
        return;
    }
    // Centered title on top.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default().fg(Color::Gray),
        )))
        .alignment(Alignment::Center),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );
    // Large number, centered, in the remaining interior.
    if inner.height >= 2 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                value,
                Style::default()
                    .fg(value_color)
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Center),
            Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: inner.height - 1,
            },
        );
    }
}

/// 5-line glyph of a wordmark letter (hand-made block font).
fn brand_glyph(c: char) -> [&'static str; 5] {
    match c {
        'G' => ["█████", "█    ", "█ ███", "█   █", "█████"],
        'U' => ["█   █", "█   █", "█   █", "█   █", "█████"],
        'A' => ["█████", "█   █", "█████", "█   █", "█   █"],
        'R' => ["█████", "█   █", "█████", "█  █ ", "█   █"],
        'D' => ["████ ", "█   █", "█   █", "█   █", "████ "],
        'I' => ["█████", "  █  ", "  █  ", "  █  ", "█████"],
        'N' => ["█   █", "██  █", "█ █ █", "█  ██", "█   █"],
        'B' => ["████ ", "█   █", "████ ", "█   █", "████ "],
        'S' => ["█████", "█    ", "█████", "    █", "█████"],
        'E' => ["█████", "█    ", "████ ", "█    ", "█████"],
        'T' => ["█████", "  █  ", "  █  ", "  █  ", "  █  "],
        'L' => ["█    ", "█    ", "█    ", "█    ", "█████"],
        _ => ["     ", "     ", "     ", "     ", "     "],
    }
}

/// Builds the wordmark (5 lines) from the text, with letters separated by a space.
fn brand_banner(text: &str) -> Vec<String> {
    let glyphs: Vec<[&str; 5]> = text.chars().map(brand_glyph).collect();
    (0..5)
        .map(|row| glyphs.iter().map(|g| g[row]).collect::<Vec<_>>().join(" "))
        .collect()
}

/// Width (in cells) of the block banner for `text` (5 cols/letter + 1 space).
fn brand_banner_width(text: &str) -> u16 {
    let n = text.chars().count() as u16;
    if n == 0 { 0 } else { n * 5 + (n - 1) }
}

/// Draws the block banner for `text` starting at (x, y), one line at a time.
fn draw_banner(frame: &mut Frame, x: u16, y: u16, text: &str, style: Style) {
    for (i, row) in brand_banner(text).into_iter().enumerate() {
        let w = row.chars().count() as u16;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(row, style))),
            Rect {
                x,
                y: y + i as u16,
                width: w,
                height: 1,
            },
        );
    }
}

/// Builds the `ratatui-image` `Picker`: detects the graphics protocol and cell
/// size (must run **on the alternate screen**). Allows forcing the protocol
/// (`GUARDIAN_TUI_PROTO=sixel|kitty|iterm2|halfblocks`) and the cell size in px
/// (`GUARDIAN_TUI_FONT=WIDTHxHEIGHT`). Writes a diagnostics file.
fn build_picker() -> ratatui_image::picker::Picker {
    use ratatui_image::picker::{Picker, ProtocolType};
    let queried = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let mut proto = queried.protocol_type();
    if let Ok(p) = std::env::var("GUARDIAN_TUI_PROTO") {
        proto = match p.to_lowercase().as_str() {
            "sixel" => ProtocolType::Sixel,
            "kitty" => ProtocolType::Kitty,
            "iterm2" | "iterm" => ProtocolType::Iterm2,
            "halfblocks" | "blocks" => ProtocolType::Halfblocks,
            _ => proto,
        };
    }
    // Manual override of the cell size (px).
    let font_override = std::env::var("GUARDIAN_TUI_FONT")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once(['x', 'X'])?;
            Some((w.trim().parse::<u16>().ok()?, h.trim().parse::<u16>().ok()?))
        })
        .filter(|(w, h)| *w > 0 && *h > 0);

    // REAL cell size via the terminal's window_size (pixels ÷ grid) — usually
    // works where ratatui-image's font query fails (falling back to the 10x20 default).
    let ws = ratatui::crossterm::terminal::window_size().ok();
    let ws_font = ws.as_ref().and_then(|w| {
        if w.width > 0 && w.height > 0 && w.columns > 0 && w.rows > 0 {
            let (fw, fh) = (w.width / w.columns, w.height / w.rows);
            (fw > 0 && fh > 0).then_some((fw, fh))
        } else {
            None
        }
    });

    let queried_fs = queried.font_size();
    // Priority: manual override > window_size > ratio (cell_ratio) over width 10.
    // Since the terminal doesn't report the cell, the height/width ratio (adjustable
    // via GUARDIAN_TUI_CELL_RATIO) is what controls the Sixel aspect.
    let chosen = font_override
        .or(ws_font)
        .unwrap_or_else(|| (10, (10.0 * cell_ratio()).round().clamp(4.0, 60.0) as u16));

    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(ratatui_image::FontSize::new(chosen.0, chosen.1));
    picker.set_protocol_type(proto);
    picker.set_background_color(Some(image::Rgba([0x18u8, 0x1A, 0x1B, 0xFF])));

    let _ = std::fs::write(
        "guardian-tui-logo-debug.txt",
        format!(
            "protocol={:?} · CHOSEN cell {}x{}px\n\
             query_font={}x{} · window_size={:?} · ws_font={:?} · override={:?}\n\
             (force: GUARDIAN_TUI_PROTO=sixel|halfblocks · GUARDIAN_TUI_FONT=WxH)\n",
            picker.protocol_type(),
            chosen.0,
            chosen.1,
            queried_fs.width,
            queried_fs.height,
            ws.map(|w| (w.columns, w.rows, w.width, w.height)),
            ws_font,
            font_override,
        ),
    );
    picker
}

/// **Height ÷ width** ratio of the terminal cell (for the block fallback).
/// ~2.0 is the default; adjustable via `GUARDIAN_TUI_CELL_RATIO` (e.g. "2.2").
fn cell_ratio() -> f32 {
    std::env::var("GUARDIAN_TUI_CELL_RATIO")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|r| *r > 0.5 && *r < 5.0)
        .unwrap_or(2.0)
}

/// **Quadrant** glyphs (2×2 subpixels) by 4-bit mask
/// (bit0=top-left, bit1=top-right, bit2=bottom-left, bit3=bottom-right).
const QUADRANTS: [&str; 16] = [
    " ", "\u{2598}", "\u{259D}", "\u{2580}", // ' ' ▘ ▝ ▀
    "\u{2596}", "\u{258C}", "\u{259E}", "\u{259B}", // ▖ ▌ ▞ ▛
    "\u{2597}", "\u{259A}", "\u{2590}", "\u{259C}", // ▗ ▚ ▐ ▜
    "\u{2584}", "\u{2599}", "\u{259F}", "\u{2588}", // ▄ ▙ ▟ █
];

/// Draws an image as **quadrant-block** art (2×2 subpixels per cell → double the
/// horizontal resolution of half-block), **preserving the aspect** (via
/// `cell_ratio`) and centered. Each cell picks the 2 colors (fg/bg) that best
/// represent its 4 subpixels and the corresponding quadrant glyph. Returns the
/// number of lines drawn.
fn draw_image_blocks(frame: &mut Frame, area: Rect, img: &image::DynamicImage) -> u16 {
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let (iw, ih) = (img.width().max(1) as f32, img.height().max(1) as f32);
    let r = cell_ratio(); // cell height/width

    // Cells (aspect preserved): the area is area.width wide and area.height*r tall
    // in "cell-width units".
    let scale = ((area.width as f32) / iw).min((area.height as f32 * r) / ih);
    let cells_w = ((iw * scale).round() as u16).clamp(1, area.width);
    let cells_h = (((ih * scale) / r).round() as u16).clamp(1, area.height);

    // Canvas of 2·cells_w × 2·cells_h subpixels. The correct aspect is already baked
    // into cells_w/cells_h, so `resize_exact` (which fills the grid) doesn't distort
    // the content — it just subdivides each cell uniformly into 2×2.
    let (pw, ph) = (cells_w as u32 * 2, cells_h as u32 * 2);
    let rgba = img
        .resize_exact(pw, ph, image::imageops::FilterType::Triangle)
        .to_rgba8();
    const BG: [f32; 3] = [0x18 as f32, 0x1A as f32, 0x1B as f32];
    let at = |x: u32, y: u32| -> [f32; 3] {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let a = a as f32 / 255.0;
        [
            r as f32 * a + BG[0] * (1.0 - a),
            g as f32 * a + BG[1] * (1.0 - a),
            b as f32 * a + BG[2] * (1.0 - a),
        ]
    };
    let dist = |a: &[f32; 3], b: &[f32; 3]| {
        (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
    };
    let to_col = |c: &[f32; 3]| Color::Rgb(c[0] as u8, c[1] as u8, c[2] as u8);

    let x0 = area.x + area.width.saturating_sub(cells_w) / 2;
    let y0 = area.y + area.height.saturating_sub(cells_h) / 2;
    let buf = frame.buffer_mut();
    for cy in 0..cells_h {
        for cx in 0..cells_w {
            let (px, py) = (cx as u32 * 2, cy as u32 * 2);
            // Subpixels in the order TL, TR, BL, BR (= bits 0,1,2,3).
            let sub = [
                at(px, py),
                at(px + 1, py),
                at(px, py + 1),
                at(px + 1, py + 1),
            ];
            // 2 seed colors = the most distant pair of subpixels.
            let (mut i0, mut i1, mut best) = (0usize, 1usize, -1.0f32);
            for a in 0..4 {
                for b in (a + 1)..4 {
                    let d = dist(&sub[a], &sub[b]);
                    if d > best {
                        best = d;
                        i0 = a;
                        i1 = b;
                    }
                }
            }
            let (s0, s1) = (sub[i0], sub[i1]);
            // Each subpixel goes to the nearest seed; average of each group.
            let (mut fg, mut bg) = ([0f32; 3], [0f32; 3]);
            let (mut nf, mut nb, mut mask) = (0f32, 0f32, 0u8);
            for (k, s) in sub.iter().enumerate() {
                if dist(s, &s1) < dist(s, &s0) {
                    fg[0] += s[0];
                    fg[1] += s[1];
                    fg[2] += s[2];
                    nf += 1.0;
                    mask |= 1 << k;
                } else {
                    bg[0] += s[0];
                    bg[1] += s[1];
                    bg[2] += s[2];
                    nb += 1.0;
                }
            }
            let fg_col = if nf > 0.0 {
                to_col(&[fg[0] / nf, fg[1] / nf, fg[2] / nf])
            } else {
                to_col(&BG)
            };
            let bg_col = if nb > 0.0 {
                to_col(&[bg[0] / nb, bg[1] / nb, bg[2] / nb])
            } else {
                to_col(&BG)
            };
            if let Some(cell) = buf.cell_mut((x0 + cx, y0 + cy)) {
                cell.set_symbol(QUADRANTS[mask as usize])
                    .set_fg(fg_col)
                    .set_bg(bg_col);
            }
        }
    }
    cells_h
}

/// Startup splash mode (`GUARDIAN_TUI_SPLASH`):
/// - `logo` (default): PNG logo in half-block art (custom renderer, aspect
///   preserved — adjustable via `GUARDIAN_TUI_CELL_RATIO`);
/// - `sentinel`: "GUARDIAN SENTINEL" wordmark in coral blocks with an outline;
/// - `glyph`: brand-mark in Nerd Font glyphs;
/// - `ascii`: crest in ASCII art (universal fallback).
fn render_connecting(frame: &mut Frame, area: Rect, app: &App) {
    match std::env::var("GUARDIAN_TUI_SPLASH").ok().as_deref() {
        Some("sentinel") => render_connecting_sentinel(frame, area),
        Some("glyph") => render_connecting_glyph(frame, area),
        Some("ascii") => render_connecting_ascii(frame, area),
        _ => render_connecting_logo(frame, area, app),
    }
}

/// "GUARDIAN SENTINEL" splash: wordmark in blocks in the brand coral (#D97757),
/// on two lines, with an offset outline/shadow that mimics the double outline of
/// the reference art. No dependency on a special font or an image.
fn render_connecting_sentinel(frame: &mut Frame, area: Rect) {
    let fill = Style::default().fg(BRAND_GOLD).add_modifier(Modifier::BOLD);
    let outline = Style::default().fg(BRAND_GOLD_DIM);
    let w = brand_banner_width("GUARDIAN"); // == "SENTINEL" (8 letras) = 47
    let block_h = 5u16;
    let gap = 1u16;

    // Compact fallback if the screen is too narrow/short for the banner.
    if area.width < w + 2 || area.height < block_h * 2 + gap + 4 {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled("GUARDIAN SENTINEL", fill)),
            Line::from(""),
            Line::from(Span::styled(
                "⏳ Initializing the P2P node…",
                Style::default().fg(GOLD_GRADIENT[1]),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
        return;
    }

    let x = area.x + (area.width - w) / 2;
    let total = block_h * 2 + gap;
    let y = area.y + area.height.saturating_sub(total + 4) / 2;

    // Each line: first the outline (offset +1,+1), then the fill on top — leaving
    // a dark-coral "fringe" on the right/bottom edges (the outline).
    draw_banner(frame, x + 1, y + 1, "GUARDIAN", outline);
    draw_banner(frame, x, y, "GUARDIAN", fill);
    let y2 = y + block_h + gap;
    draw_banner(frame, x + 1, y2 + 1, "SENTINEL", outline);
    draw_banner(frame, x, y2, "SENTINEL", fill);

    // Captions below the wordmark.
    let ty = y2 + block_h + 1;
    if ty < area.y + area.height {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Administration Panel · sovereign P2P database",
                    Style::default().fg(GOLD_GRADIENT[1]),
                )),
                Line::from(Span::styled(
                    "⏳ Initializing the P2P node (IrohClient + GuardianDB)…",
                    Style::default().fg(GOLD_GRADIENT[3]),
                )),
            ])
            .alignment(Alignment::Center),
            Rect {
                x: area.x,
                y: ty,
                width: area.width,
                height: (area.y + area.height - ty).min(2),
            },
        );
    }
}

/// Splash with a brand-mark in **Nerd Font glyphs**: a "gear · shield · gear"
/// emblem (the logo motif) in a gold badge, the GUARDIANDB wordmark and the
/// captions. Lightweight and image-free — requires a Nerd Font in the terminal.
fn render_connecting_glyph(frame: &mut Frame, area: Rect) {
    let gold = |i: usize| {
        Style::default()
            .fg(GOLD_GRADIENT[i.min(4)])
            .add_modifier(Modifier::BOLD)
    };
    let edge = Style::default().fg(BRAND_GOLD_DIM);
    let cog = "\u{f013}"; // nf-fa-cog (gear)
    let shield = "\u{f132}"; // nf-fa-shield (shield/guardian)

    let mut lines: Vec<Line> = vec![Line::from("")];
    // Badge with the emblem (the three lines have the same width → they center together).
    lines.push(Line::from(Span::styled("┌───────────────┐", edge)));
    lines.push(Line::from(vec![
        Span::styled("│   ", edge),
        Span::styled(format!("{cog}   {shield}   {cog}"), gold(2)),
        Span::styled("   │", edge),
    ]));
    lines.push(Line::from(Span::styled("└───────────────┘", edge)));
    lines.push(Line::from(""));
    // Wordmark in block art (font-independent), with a gold gradient.
    for (i, row) in brand_banner("GUARDIANDB").into_iter().enumerate() {
        lines.push(Line::from(Span::styled(row, gold(i))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Administration Panel · sovereign P2P database",
        Style::default().fg(GOLD_GRADIENT[2]),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "⏳ Initializing the P2P node (IrohClient + GuardianDB)…",
        Style::default().fg(GOLD_GRADIENT[4]),
    )));

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

/// Renders a `ratatui-image` protocol (crisp image via the terminal's graphics
/// protocol), preserving aspect (Fit) and centered in the area.
fn draw_protocol_centered(
    frame: &mut Frame,
    area: Rect,
    proto: &mut ratatui_image::protocol::StatefulProtocol,
) {
    let fitted = proto.size_for(
        ratatui_image::Resize::Fit(None),
        ratatui::layout::Size::new(area.width, area.height),
    );
    let cw = fitted.width.min(area.width);
    let ch = fitted.height.min(area.height);
    let centered = Rect {
        x: area.x + (area.width - cw) / 2,
        y: area.y + (area.height - ch) / 2,
        width: cw,
        height: ch,
    };
    frame.render_stateful_widget(ratatui_image::StatefulImage::default(), centered, proto);
}

/// Logo splash: **crisp image** via a graphics protocol (Sixel/Kitty/iTerm2)
/// when available; otherwise a quadrant-block fallback. Caption below.
fn render_connecting_logo(frame: &mut Frame, area: Rect, app: &App) {
    // Reserve 3 lines at the bottom for the captions; the rest is the image area.
    let legend_h: u16 = 3;
    let img_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(legend_h + 1),
    };
    let legend_y = img_area.y + img_area.height + 1;

    let drew = app.graphics && {
        let mut guard = app.logo_proto.borrow_mut();
        match guard.as_mut() {
            Some(proto) => {
                draw_protocol_centered(frame, img_area, proto);
                true
            }
            None => false,
        }
    };
    if !drew {
        match &app.logo_img {
            Some(img) => {
                draw_image_blocks(frame, img_area, img);
            }
            None => {
                render_connecting_ascii(frame, area);
                return;
            }
        }
    }

    // Captions below the image.
    if legend_y < area.y + area.height {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Administration Panel · sovereign P2P database",
                    Style::default().fg(GOLD_GRADIENT[2]),
                )),
                Line::from(Span::styled(
                    "⏳ Initializing the P2P node (IrohClient + GuardianDB)…",
                    Style::default().fg(GOLD_GRADIENT[4]),
                )),
            ])
            .alignment(Alignment::Center),
            Rect {
                x: area.x,
                y: legend_y,
                width: area.width,
                height: (area.y + area.height - legend_y).min(2),
            },
        );
    }
}

/// Image-free splash fallback: crest + wordmark in ASCII art (gold gradient).
fn render_connecting_ascii(frame: &mut Frame, area: Rect) {
    // Emblem (gold gear + helm), symmetric lines to center nicely.
    let emblem = [
        "╔═╦═════╦═╗",
        "║ ║ ▟█▙ ║ ║",
        "╠═╣ ███ ╠═╣",
        "║ ║ ▜█▛ ║ ║",
        "╚═╩═════╩═╝",
    ];

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    // Emblem with a gold gradient.
    for (i, row) in emblem.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            *row,
            Style::default()
                .fg(GOLD_GRADIENT[i.min(4)])
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    // "GUARDIANDB" wordmark — each line in a gradient tone.
    for (i, row) in brand_banner("GUARDIANDB").into_iter().enumerate() {
        lines.push(Line::from(Span::styled(
            row,
            Style::default()
                .fg(GOLD_GRADIENT[i.min(4)])
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "════════════════════════════",
        Style::default().fg(GOLD_GRADIENT[3]),
    )));
    lines.push(Line::from(Span::styled(
        "Administration Panel · sovereign P2P database",
        Style::default().fg(GOLD_GRADIENT[2]),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "⏳ Initializing the P2P node (IrohClient + GuardianDB)…",
        Style::default().fg(GOLD_GRADIENT[4]),
    )));

    let paragraph = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .block(Block::default());

    frame.render_widget(paragraph, area);
}

fn render_dashboard(frame: &mut Frame, area: Rect, app: &App) {
    // Layout: metrics panel (9: header + cards + status) | stores (flex)
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // metrics (header takes 2 lines)
            Constraint::Min(3),    // stores
        ])
        .split(area);

    // Counters per type
    let eventlog_count = app
        .stores
        .iter()
        .filter(|s| s.store_type == "eventlog")
        .count();
    let kv_count = app
        .stores
        .iter()
        .filter(|s| s.store_type == "keyvalue")
        .count();
    let doc_count = app
        .stores
        .iter()
        .filter(|s| s.store_type == "document")
        .count();
    let total_entries: usize = app.stores.iter().map(|s| s.entry_count).sum();
    let syncing_count = app
        .stores
        .iter()
        .filter(|s| s.sync_status == SyncStatus::Syncing)
        .count();
    let error_count_stores = app
        .stores
        .iter()
        .filter(|s| s.sync_status == SyncStatus::Error)
        .count();

    // Metrics panel: "METRICS" section with 6 cards (title + number) and, below,
    // a status line.
    let metrics_inner = draw_headed_panel(frame, layout[0], "METRICS");
    let mrows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(1)])
        .split(metrics_inner);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 6); 6])
        .split(mrows[0]);

    let cards: [(&str, String, Color); 6] = [
        ("OPEN STORES", app.stores.len().to_string(), Color::White),
        ("LOGS (EVENTLOG)", eventlog_count.to_string(), Color::Blue),
        ("KEYVALUE STORES", kv_count.to_string(), Color::Green),
        ("DOCUMENT STORES", doc_count.to_string(), Color::Magenta),
        ("TOTAL ENTRIES", total_entries.to_string(), Color::White),
        ("SYNCS (TOTAL)", app.syncs_total.to_string(), Color::Green),
    ];
    for (i, (title, value, color)) in cards.into_iter().enumerate() {
        draw_metric_card(frame, cols[i], title, &value, color);
    }

    // Status line below the cards.
    let status_line = Line::from(vec![
        Span::styled(" Syncing: ", Style::default().fg(Color::Gray)),
        Span::styled(
            syncing_count.to_string(),
            Style::default().fg(if syncing_count > 0 {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled("   │   Store errors: ", Style::default().fg(Color::Gray)),
        Span::styled(
            error_count_stores.to_string(),
            Style::default().fg(if error_count_stores > 0 {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled("   │   Errors: ", Style::default().fg(Color::Gray)),
        Span::styled(
            app.sync_errors.to_string(),
            Style::default().fg(if app.sync_errors > 0 {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled("   │   Source: ", Style::default().fg(Color::Gray)),
        Span::styled(
            app.source_label.clone(),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(status_line), mrows[1]);

    // List title with the active filter
    let filter_label = app.store_filter.label();
    let filtered = app.filtered_stores();
    let store_title = if app.store_filter == StoreFilter::All {
        format!(" STORES ({}) ", app.stores.len())
    } else {
        format!(
            " STORES — {} ({}/{}) ",
            filter_label,
            filtered.len(),
            app.stores.len()
        )
    };

    // Store list
    if filtered.is_empty() {
        let msg = if app.stores.is_empty() {
            vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    " No store open.",
                    Style::default().fg(Color::DarkGray),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Press ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        "n",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " to create your first store (EventLog, KeyValue or Document).",
                        Style::default().fg(Color::Gray),
                    ),
                ]),
                Line::from(vec![Span::styled(
                    " The created store reopens automatically on restart.",
                    Style::default().fg(Color::DarkGray),
                )]),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!(
                        " No store of type '{}' found. Press Tab to change the filter.",
                        filter_label
                    ),
                    Style::default().fg(Color::DarkGray),
                )]),
            ]
        };

        let empty_inner = draw_headed_panel(frame, layout[1], &store_title);
        let empty = Paragraph::new(msg);
        frame.render_widget(empty, empty_inner);
    } else {
        let items: Vec<ListItem> = filtered
            .iter()
            .map(|s| {
                let type_color = match s.store_type.as_str() {
                    "eventlog" => Color::Blue,
                    "keyvalue" => Color::Green,
                    "document" => Color::Magenta,
                    _ => Color::Gray,
                };

                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {} ", s.sync_status.icon()),
                        Style::default().fg(s.sync_status.color()),
                    ),
                    Span::styled(
                        format!("{:>10} ", s.store_type),
                        Style::default().fg(type_color),
                    ),
                    Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&s.db_name, Style::default().fg(Color::White)),
                    Span::styled(
                        format!("  ({} entries)", s.entry_count),
                        Style::default().fg(Color::DarkGray),
                    ),
                    if s.sync_status == SyncStatus::Syncing {
                        Span::styled(
                            format!("  [{}/{}]", s.replication_progress, s.replication_max),
                            Style::default().fg(Color::Yellow),
                        )
                    } else {
                        Span::styled("", Style::default())
                    },
                ]))
            })
            .collect();

        let list_inner = draw_headed_panel(frame, layout[1], &store_title);
        let store_list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(BRAND_GOLD)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");

        frame.render_stateful_widget(store_list, list_inner, &mut app.store_list_state.clone());
    }
}

fn render_acl_manager(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .acls
        .iter()
        .map(|a| {
            let roles_txt = a
                .roles
                .iter()
                .map(|r| {
                    let keys = if r.keys.is_empty() {
                        "-".to_string()
                    } else {
                        r.keys
                            .iter()
                            .map(|k| preview(k, 10))
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    format!("{}[{}]", r.role, keys)
                })
                .collect::<Vec<_>>()
                .join("  ");
            let (ctype_color, ctype_icon) = controller_type_style(&a.controller_type);
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {ctype_icon} {:<8} ", preview(&a.controller_type, 8)),
                    Style::default()
                        .fg(ctype_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} ", preview(&a.store, 24)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(preview(&roles_txt, 44), Style::default().fg(Color::Gray)),
            ]))
        })
        .collect();
    let title = format!(" Access Control ({} controllers) ", app.acls.len());
    render_inspector_list(
        frame,
        area,
        title,
        items,
        "No controllers — press 'n' to create one.",
        &app.inspector_state,
    );
}

fn render_keystore_manager(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .keystore_keys
        .iter()
        .map(|k| {
            ListItem::new(Line::from(vec![
                Span::styled(" 🔑 ", Style::default().fg(Color::Yellow)),
                Span::styled(preview(k, 60), Style::default().fg(Color::White)),
            ]))
        })
        .collect();
    let title = format!(" Keystore ({} keys) ", app.keystore_keys.len());
    render_inspector_list(
        frame,
        area,
        title,
        items,
        "No keys — press 'n' to generate one (only metadata/public key are shown).",
        &app.inspector_state,
    );
}

/// Contextual help per screen (G4.1), in plain English for those unfamiliar with
/// the concepts. Returns `(title, body)`. Lines ending in `:` become section
/// headers in the render.
fn help_for_screen(screen: &Screen) -> (String, String) {
    let common = "\nGeneral navigation:\n\
        • F1 Dashboard · F2 Topology · F3 Network · F4 Access · F5 Keystore · F6 Blobs · F7 Events\n\
        • Enter opens · Esc goes back · ↑↓ arrows navigate · q quits · ? shows this help";

    let (title, body): (&str, String) = match screen {
        Screen::Dashboard => (
            "Dashboard (stores)",
            format!(
                "What it is: the list of your \"stores\" (databases). Each store holds data in\n\
                 a different way:\n\
                 • EventLog — an append-only history (like an immutable ledger).\n\
                 • KeyValue — key→value pairs (like a dictionary).\n\
                 • Document — JSON documents with an id (like a collection/table).\n\
                 \n\
                 What you can do here:\n\
                 • n — create a new store (guided wizard).\n\
                 • Enter — open the selected store and view/edit the data.\n\
                 • s — share the store: generates \"tickets\" for a friend to replicate.\n\
                 • i — import a store from a ticket you received.\n\
                 • y — show your identity (NodeId) for sharing.\n\
                 • x — close the store (keeps the data; reopens on restart).\n\
                 • d — drop the store (DELETES the local data; asks for confirmation).\n\
                 • l — audit trail (this session's administration actions).\n\
                 \n\
                 Everything you create here reopens on its own when you restart the panel.{common}"
            ),
        ),
        Screen::EventLogInspector { .. } => (
            "EventLog Inspector",
            format!(
                "What it is: the entries of an append-only log, from oldest (top) to newest.\n\
                 Entries are never edited or deleted — only appended.\n\
                 \n\
                 What you can do here:\n\
                 • a — append a new entry.\n\
                 • Enter — see the entry's full details (hash, clock, author, payload).\n\
                 • / — search by text · t — filter by logical \"clock\" (min-max).\n\
                 • h — see the CRDT \"heads\" (tips) and merge divergences.\n\
                 • scrolling to the top loads more old history automatically.\n\
                 \n\
                 Concept: a \"logical clock\" (Lamport) is an event-ordering counter, not the\n\
                 wall-clock time.{common}"
            ),
        ),
        Screen::KeyValueInspector { .. } => (
            "KeyValue Inspector",
            format!(
                "What it is: a store of key→value pairs, like a dictionary. Writing a key\n\
                 replicates automatically to the connected peers.\n\
                 \n\
                 What you can do here:\n\
                 • n — new key (key=value format; the value can be text or JSON).\n\
                 • e — edit the value of the selected key.\n\
                 • d — delete the selected key (asks for confirmation).\n\
                 • Enter — see the full value · / — search · x — export everything as JSON.{common}"
            ),
        ),
        Screen::DocumentInspector { .. } => (
            "Document Inspector",
            format!(
                "What it is: a collection of JSON documents, each with an id. Like a table in\n\
                 a document database.\n\
                 \n\
                 What you can do here:\n\
                 • n — new document (id={{\"field\":\"value\"}} format).\n\
                 • d — delete the selected document (asks for confirmation).\n\
                 • Enter — see the full document (formatted JSON) · / — search.{common}"
            ),
        ),
        Screen::AccessControlManager => (
            "Access Control (ACL)",
            format!(
                "What it is: who can write to each store. A \"controller\" lists the authorized\n\
                 keys by role (admin/write).\n\
                 \n\
                 What you can do here:\n\
                 • n — create a new controller (wizard).\n\
                 • g — grant access to a key · x — revoke (Tab toggles write/admin).\n\
                 • Enter — see all of the controller's permissions.\n\
                 \n\
                 Concept: a \"key\" here is a peer's public identity (its NodeId).\n\
                 Authorizing a friend's key lets them write to the shared store.\n\
                 \n\
                 Known limitation: today the core treats every controller type as\n\
                 \"simple\" — the type is recorded in the manifest, but the behavior is the same.{common}"
            ),
        ),
        Screen::KeystoreManager => (
            "Keystore (keys)",
            format!(
                "What it is: the cryptographic keys managed by the panel. The PRIVATE key is never\n\
                 shown — only the public one (which you can share).\n\
                 \n\
                 What you can do here:\n\
                 • n — generate a new key (Ed25519).\n\
                 • r — rotate (generates a new pair under the same id; counts as a rotation).\n\
                 • Enter/x — see the public key + metadata (active/rotated, age).{common}"
            ),
        ),
        Screen::ReplicationMonitor => (
            "Replication Monitor",
            format!(
                "What it is: the state of synchronization with other peers (nodes) on the network.\n\
                 \n\
                 What you can do here:\n\
                 • Enter — peer detail (shared stores, recent syncs).\n\
                 • s — force a sync with the selected peer · c — connect to a NodeId.\n\
                 \n\
                 Concept: a \"sync\" is the exchange of data between nodes so they match. A peer with\n\
                 no recent sync is flagged (◐ yellow).{common}"
            ),
        ),
        Screen::NetworkTopology => (
            "Network Topology",
            format!(
                "What it is: the map of your connections — this node at the center, each peer as an edge.\n\
                 Shows the link type (direct/relay), latency, relay used and throughput.\n\
                 \n\
                 Concepts:\n\
                 • direct — point-to-point connection · relay — via an intermediary network server.\n\
                 • p95/p99 — the typical latency of the 95%/99% worst cases.\n\
                 • \"known offline\" — peers you've seen before but that aren't connected right now.{common}"
            ),
        ),
        Screen::EventBusExplorer => (
            "EventBus (live events)",
            format!(
                "What it is: the database's internal event stream in real time (syncs, connections,\n\
                 store updates).\n\
                 \n\
                 What you can do here:\n\
                 • f — follow (auto-scroll) · Space — pause · t — filter by kind · / — search.\n\
                 The header shows the events/s rate, an activity sparkline and the top peers.{common}"
            ),
        ),
        Screen::BlobBrowser => (
            "Blobs (files)",
            format!(
                "What it is: the \"blobs\" — pieces of content addressed by hash (BLAKE3),\n\
                 typically files transferred via P2P.\n\
                 \n\
                 What you can do here:\n\
                 • a — add a blob from a file · x — export to a file · d — delete.\n\
                 • s — toggle sorting (hash/size) · Enter — see detail + preview.\n\
                 \n\
                 The green ● = complete blob; yellow ◐ = partial download.{common}"
            ),
        ),
        Screen::StoreDetail { .. } => (
            "Store Detail",
            format!(
                "What it is: a store's metadata (name, type, address, number of entries) and the\n\
                 peers observed syncing with it.\n\
                 • c — connect/sync a peer to this store.{common}"
            ),
        ),
        _ => ("Help", format!("Guardian-DB administration panel.{common}")),
    };
    (title.to_string(), body)
}

fn render_placeholder(frame: &mut Frame, area: Rect, screen: &Screen) {
    let screen_name = match screen {
        Screen::StoreDetail { store_address } => format!("Store: {store_address}"),
        Screen::EventLogInspector { log_name } => format!("EventLog: {log_name}"),
        Screen::KeyValueInspector { kv_name } => format!("KeyValue: {kv_name}"),
        Screen::DocumentInspector { store_name } => format!("Document: {store_name}"),
        Screen::AccessControlManager => "Access Control Manager".into(),
        Screen::AccessControlDetail { controller_id } => format!("ACL: {controller_id}"),
        Screen::ReplicationMonitor => "Replication Monitor".into(),
        Screen::PeerDetail { node_id } => format!("Peer: {node_id}"),
        Screen::NetworkTopology => "Network Topology".into(),
        Screen::EventBusExplorer => "EventBus Explorer".into(),
        Screen::KeystoreManager => "Keystore Manager".into(),
        Screen::KeyDetail { key_id } => format!("Key: {key_id}"),
        Screen::BlobBrowser => "Blob Browser".into(),
        Screen::BlobDetail { hash } => format!("Blob: {hash}"),
        _ => "Unknown".into(),
    };

    let text = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("🚧 {screen_name}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "This screen will be implemented in future phases.",
            Style::default().fg(Color::DarkGray),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Press Esc to go back.",
            Style::default().fg(Color::Gray),
        )]),
    ];

    let inner = draw_gold_panel(frame, area, &screen_name);
    let paragraph = Paragraph::new(text).alignment(Alignment::Center);
    frame.render_widget(paragraph, inner);
}

/// Contextual shortcuts (key, label) of the current screen. Extracted to be
/// shared between drawing the footer and computing its height.
fn footer_shortcuts(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.screen {
        Screen::Connecting => vec![("q", "Quit")],
        Screen::ConnectionFailed { .. } => vec![("q", "Quit")],
        // Primary bar (curated, fits on one line). The secondary keys
        // (s Share, i Import, x Close, d Drop, y ID, l Log, ? Help)
        // remain active — they are listed under "? Help".
        Screen::Dashboard => vec![
            ("F1", "Dashboard"),
            ("F2", "Topology"),
            ("F3", "Network"),
            ("F4", "Access"),
            ("F5", "Keystore"),
            ("F6", "Blobs"),
            ("F7", "Events"),
            ("\u{2191}\u{2193}", "Navigate"),
            ("Enter", "Open"),
            ("n", "New Store"),
            ("q", "Quit"),
        ],
        Screen::KeyValueInspector { .. } => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("Enter", "Detail"),
            ("e", "Edit"),
            ("n", "New"),
            ("d", "Delete"),
            ("/", "Search"),
            ("x", "Export"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::DocumentInspector { .. } => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("Enter", "View doc"),
            ("n", "New"),
            ("d", "Delete"),
            ("/", "Search"),
            ("r", "Refresh"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::AccessControlManager => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("Enter", "Detail"),
            ("g", "Grant"),
            ("x", "Revoke"),
            ("n", "New"),
            ("r", "Refresh"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::ReplicationMonitor => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("Enter", "Detail"),
            ("s", "Sync"),
            ("c", "Connect"),
            ("r", "Refresh"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::EventLogInspector { .. } => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("\u{2191}@top", "+Older"),
            ("Enter", "Detail"),
            ("a", "Append"),
            ("/", "Search"),
            ("t", "Clock"),
            ("h", "Heads"),
            ("r", "Refresh"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::BlobBrowser => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("Enter", "Detail"),
            ("s", "Sort"),
            ("a", "Add"),
            ("x", "Export"),
            ("d", "Delete"),
            ("r", "Refresh"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::KeystoreManager => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("Enter/x", "Public key"),
            ("n", "Generate"),
            ("r", "Rotate"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::NetworkTopology => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("r", "Refresh"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::EventBusExplorer => vec![
            ("\u{2191}\u{2193}", "Navigate"),
            ("f", "Follow"),
            ("Spc", "Pause"),
            ("t", "Filter"),
            ("/", "Search"),
            ("c", "Clear"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        Screen::StoreDetail { .. } => {
            vec![("c", "Connect peer"), ("Esc", "Back"), ("q", "Quit")]
        }
        _ => vec![("Esc", "Back"), ("q", "Quit")],
    }
}

/// Width (in cells) of a footer chip in the format ` {desc} ({key}) `.
fn footer_box_width(key: &str, desc: &str) -> u16 {
    (desc.chars().count() + key.chars().count()) as u16 + 5
}

/// Height (in lines) of a footer chip (one line).
const FOOTER_ROW_H: u16 = 1;

/// Breaks the shortcuts into rows greedily, fitting as many per row as possible
/// within `width`. Returns, per row, the indices of that row's shortcuts.
fn footer_layout_rows(shortcuts: &[(&str, &str)], width: u16) -> Vec<Vec<usize>> {
    let gap: u16 = 1;
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut used: u16 = 0;
    for (i, (key, desc)) in shortcuts.iter().enumerate() {
        let w = footer_box_width(key, desc);
        let empty = rows.last().map(|r| r.is_empty()).unwrap_or(true);
        let need = if empty { w } else { w + gap };
        if !empty && used + need > width {
            rows.push(vec![i]);
            used = w;
        } else {
            rows.last_mut().unwrap().push(i);
            used += need;
        }
    }
    rows
}

/// True when the footer is in an "override" mode (search/input/confirmation/notification),
/// which uses a single line instead of the button rows.
fn footer_override_active(app: &App) -> bool {
    app.searching || app.input.is_some() || app.confirm.is_some() || app.notification.is_some()
}

/// Desired footer height for the current screen/state. Overrides use 3 lines;
/// otherwise it is (number of button rows × box height) + 1 log line.
fn footer_desired_height(app: &App, width: u16) -> u16 {
    if footer_override_active(app) {
        return 3;
    }
    let shortcuts = footer_shortcuts(app);
    let rows = footer_layout_rows(&shortcuts, width).len().max(1) as u16;
    rows * FOOTER_ROW_H + 3 // chip rows + log box (3 lines)
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    // Search field being edited.
    if app.searching {
        let query = app.search.as_deref().unwrap_or("");
        let line = Line::from(vec![
            Span::styled(
                " / ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {query}\u{2588}"),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                "   (Enter applies, Esc cancels)",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let p = Paragraph::new(line).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(p, area);
        return;
    }

    // The input prompt has top priority (shows label + what was typed).
    if let Some(ref inp) = app.input {
        let line = Line::from(vec![
            Span::styled(
                format!(" ✎ {}", inp.label),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}\u{2588}", inp.buffer),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                "   (Enter confirms, Esc cancels)",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let p = Paragraph::new(line).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(p, area);
        return;
    }

    // The confirmation dialog has top priority in the footer.
    if let Some(ref c) = app.confirm {
        let line = Line::from(vec![Span::styled(
            format!(" ⚠ {}", c.message),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]);
        let p = Paragraph::new(line).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(p, area);
        return;
    }

    // If there is an active notification, show it
    if let Some(ref notif) = app.notification {
        let color = if notif.is_error {
            Color::Red
        } else {
            Color::Green
        };
        let notif_line = Line::from(vec![
            Span::styled(
                if notif.is_error { " ✗ " } else { " ✓ " },
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(&notif.message, Style::default().fg(color)),
        ]);
        let p = Paragraph::new(notif_line).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(p, area);
        return;
    }

    // Contextual keyboard shortcuts
    let shortcuts = footer_shortcuts(app);

    // Buttons = " {Title} ({Key}) " text chips, filled like the wizards' Esc
    // button (black text on coral), laid out over a red band (no icons and no
    // outline). They wrap into rows when they don't fit on one line.
    let rows = footer_layout_rows(&shortcuts, area.width);
    let box_area_h = area.height.saturating_sub(3); // reserve 3 lines for the log box
    let max_rows = (box_area_h / FOOTER_ROW_H).max(1) as usize;
    let visible_rows = rows.len().min(max_rows);

    let band = Style::default().bg(HEADER_BG); // red band (header background)
    let chip = Style::default()
        .fg(Color::Black)
        .bg(BRAND_GOLD)
        .add_modifier(Modifier::BOLD); // same fill as "Esc"

    for (r, row) in rows.iter().take(visible_rows).enumerate() {
        let y = area.y + r as u16 * FOOTER_ROW_H;
        // Red band spanning the whole line (the gap between chips reveals it).
        frame.render_widget(
            Block::default().style(band),
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
        );
        let mut x = area.x;
        for &i in row {
            let (key, desc) = shortcuts[i];
            let w = footer_box_width(key, desc);
            if x + w > area.x + area.width {
                break; // extra guard: never draw beyond the available width
            }
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {desc} ({key}) "), chip))),
                Rect {
                    x,
                    y,
                    width: w,
                    height: 1,
                },
            );
            x += w + 1; // 1-cell gap (shows the band between chips)
        }
    }

    // Log/relay line inside a rectangle (square corners, no band), below the
    // button rows. Truncated by character (not by byte) so it never slices in the
    // middle of a UTF-8 codepoint and panics.
    let log_y = area.y + visible_rows as u16 * FOOTER_ROW_H;
    let avail = (area.y + area.height).saturating_sub(log_y);
    if avail >= 3 {
        let box_rect = Rect {
            x: area.x,
            y: log_y,
            width: area.width,
            height: 3,
        };
        let inner = draw_gold_panel(frame, box_rect, "");
        let log_line = app.log_buffer.get_last();
        let maxw = inner.width.saturating_sub(1) as usize;
        let chars: Vec<char> = log_line.chars().collect();
        let log_display = if chars.len() > maxw {
            format!(
                "…{}",
                chars[chars.len() - maxw.saturating_sub(1)..]
                    .iter()
                    .collect::<String>()
            )
        } else {
            log_line
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {log_display}"),
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
    }
}

// Colocated with the render helpers it exercises (input handling follows below),
// so keep it here rather than at the file end.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod render_smoke {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    /// The window has its own header (title + Esc on the 1st inner line) and separator.
    #[test]
    fn window_has_header_bar() {
        let (w, h) = (60u16, 6u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                super::draw_window(f, Rect::new(0, 0, w, h), "New Store");
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // Square box.
        assert_eq!(buf[(0u16, 0u16)].symbol(), "┌");
        // Header on the 1st inner line (y=1): title + Esc button.
        let hdr: String = (0..w).map(|x| buf[(x, 1u16)].symbol()).collect();
        assert!(hdr.contains("New Store"), "missing title: {hdr:?}");
        assert!(hdr.contains("Esc"), "missing close button: {hdr:?}");
        // Separator on the 2nd inner line (y=2), connected to the sides.
        assert_eq!(buf[(0u16, 2u16)].symbol(), "├");
        assert_eq!(buf[(w - 1, 2u16)].symbol(), "┤");
    }

    /// The panel has SQUARE corners (┌┐└┘) and the title on the top border.
    #[test]
    fn gold_panel_is_square() {
        let backend = TestBackend::new(30, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                super::draw_gold_panel(f, Rect::new(0, 0, 30, 7), "Metrics");
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0u16, 0u16)].symbol(), "┌");
        assert_eq!(buf[(29u16, 0u16)].symbol(), "┐");
        assert_eq!(buf[(0u16, 6u16)].symbol(), "└");
        assert_eq!(buf[(29u16, 6u16)].symbol(), "┘");
        // The title appears on the top border.
        let row0: String = (0..30).map(|x| buf[(x, 0u16)].symbol()).collect();
        assert!(row0.contains("Metrics"), "missing title: {row0:?}");
    }

    /// The chip width matches the text " {desc} ({key}) ".
    #[test]
    fn footer_box_width_matches_chip_text() {
        // " Dashboard (F1) " = 16 cells.
        assert_eq!(
            super::footer_box_width("F1", "Dashboard"),
            " Dashboard (F1) ".chars().count() as u16
        );
        assert_eq!(
            super::footer_box_width("Enter", "Open"),
            " Open (Enter) ".chars().count() as u16
        );
    }

    /// Buttons wrap into rows when they don't fit on one line; and all fit when there's room.
    #[test]
    fn footer_rows_wrap_by_width() {
        let sc = vec![
            ("F1", "Dashboard"),
            ("F2", "Topology"),
            ("F3", "Network"),
            ("q", "Quit"),
        ];
        // Very narrow → one box per row.
        let narrow = super::footer_layout_rows(&sc, 12);
        assert_eq!(narrow.len(), 4);
        // Very wide → everything on one row.
        let wide = super::footer_layout_rows(&sc, 200);
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0].len(), 4);
    }

    /// The footer card has all four SQUARE corners (┌┐└┘) and the key in the title.
    #[test]
    fn footer_boxes_are_square() {
        use super::{Alignment, BorderType, Borders, Modifier, Span, Style};
        use ratatui::widgets::Block;
        let w = super::footer_box_width("F1", "Dashboard");
        let backend = TestBackend::new(w, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                // Same construction as the footer.
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Plain)
                    .title(Span::styled(
                        " F1 ",
                        Style::default().add_modifier(Modifier::BOLD),
                    ))
                    .title_alignment(Alignment::Center);
                f.render_widget(block, Rect::new(0, 0, w, 4));
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0u16, 0u16)].symbol(), "┌");
        assert_eq!(buf[(w - 1, 0u16)].symbol(), "┐");
        assert_eq!(buf[(0u16, 3u16)].symbol(), "└");
        assert_eq!(buf[(w - 1, 3u16)].symbol(), "┘");
    }

    /// A metric card draws square corners, the title and the number.
    #[test]
    fn metric_card_draws_square() {
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                super::draw_metric_card(
                    f,
                    Rect::new(0, 0, 20, 4),
                    "STORES",
                    "42",
                    super::Color::White,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0u16, 0u16)].symbol(), "┌");
        assert_eq!(buf[(19u16, 3u16)].symbol(), "┘");
        // Number centered on the 3rd line (interior y+1).
        let row: String = (0..20).map(|x| buf[(x, 2u16)].symbol()).collect();
        assert!(row.contains("42"), "number not rendered: {row:?}");
    }

    /// The logo (real PNG) decodes and is drawn as truecolor half-block art,
    /// preserving the aspect (~2.29:1 → ~22 lines for 100 columns with a 1:2 cell).
    #[test]
    fn logo_draws_truecolor() {
        use ratatui::style::Color;
        let img = image::load_from_memory(include_bytes!("../../docs/guardian-sentinel-logo.png"))
            .expect("logo decodes");
        let (w, h) = (100u16, 40u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut rows_drawn = 0u16;
        terminal
            .draw(|f| {
                rows_drawn = super::draw_image_blocks(f, Rect::new(0, 0, w, h), &img);
            })
            .unwrap();
        // 100 columns · aspect 2.29 · cell 1:2 → ~22 lines.
        assert!(
            (18..=26).contains(&rows_drawn),
            "unexpected lines: {rows_drawn}"
        );
        let buf = terminal.backend().buffer();
        // Produces truecolor cells (real color, not default) with quadrant glyphs.
        let mut colored = 0;
        for y in 0..rows_drawn {
            for x in 0..w {
                if matches!(buf[(x, y)].fg, Color::Rgb(..)) {
                    colored += 1;
                }
            }
        }
        assert!(colored > 100, "too few colored cells: {colored}");
    }

    /// The "GUARDIAN SENTINEL" splash draws the wordmark in blocks with the outline.
    #[test]
    fn sentinel_splash_draws_wordmark() {
        use ratatui::style::Color;
        let (w, h) = (70u16, 20u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| super::render_connecting_sentinel(f, Rect::new(0, 0, w, h)))
            .unwrap();
        let buf = terminal.backend().buffer();
        // There are █ blocks in the brand coral (fill) and in the dark coral (outline).
        let mut fill = 0;
        let mut outline = 0;
        for y in 0..h {
            for x in 0..w {
                let c = &buf[(x, y)];
                if c.symbol() == "\u{2588}" {
                    if c.fg == super::BRAND_GOLD {
                        fill += 1;
                    } else if c.fg == super::BRAND_GOLD_DIM {
                        outline += 1;
                    }
                }
            }
        }
        assert!(fill > 200, "too few fill blocks: {fill}");
        assert!(outline > 50, "outline not drawn: {outline}");
        // The brand color is the coral #D97757 (no longer gold).
        assert_eq!(super::BRAND_GOLD, Color::Rgb(0xD9, 0x77, 0x57));
    }
}

// ═══════════════════════════════════════════════════════════
// Input Handling
// ═══════════════════════════════════════════════════════════

fn handle_key(app: &mut App, key: KeyEvent) {
    // Ignore releases
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Info modal: any key dismisses.
    if app.info_modal.is_some() {
        app.info_modal = None;
        return;
    }

    // Contextual help: any key dismisses (G4.1).
    if app.help_modal.is_some() {
        app.help_modal = None;
        return;
    }

    // Active controller creation wizard: navigate steps.
    if app.wizard.is_some() {
        handle_wizard_key(app, key);
        return;
    }

    // Active store creation wizard (G1.4): navigate steps.
    if app.store_wizard.is_some() {
        handle_store_wizard_key(app, key);
        return;
    }

    // Active store import wizard (G3.3): navigate steps.
    if app.import_wizard.is_some() {
        handle_import_wizard_key(app, key);
        return;
    }

    // Search field being edited: keys edit the query; the list filters live.
    if app.searching {
        match key.code {
            KeyCode::Char(c) => {
                if let Some(s) = app.search.as_mut() {
                    s.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = app.search.as_mut() {
                    s.pop();
                }
            }
            KeyCode::Enter => app.searching = false, // apply and leave editing
            KeyCode::Esc => {
                app.search = None; // cancel the filter
                app.searching = false;
            }
            _ => {}
        }
        // Reposition the selection at the top of the filtered result.
        if app.inspector_len() > 0 {
            app.inspector_state.select(Some(0));
        } else {
            app.inspector_state.select(None);
        }
        return;
    }

    // Active input prompt: intercepts everything (types into the buffer; Enter confirms).
    if app.input.is_some() {
        match key.code {
            // Tab toggles the role (write ↔ admin) in the grant/revoke prompts (4.3).
            KeyCode::Tab => {
                if let Some(inp) = app.input.as_mut() {
                    let toggled = match &mut inp.kind {
                        InputKind::AclGrant { role, .. } => {
                            *role = next_acl_role(role);
                            Some((true, role.clone()))
                        }
                        InputKind::AclRevoke { role, .. } => {
                            *role = next_acl_role(role);
                            Some((false, role.clone()))
                        }
                        _ => None,
                    };
                    if let Some((grant, role)) = toggled {
                        inp.label = acl_role_label(grant, &role);
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(inp) = app.input.as_mut() {
                    inp.buffer.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(inp) = app.input.as_mut() {
                    inp.buffer.pop();
                }
            }
            KeyCode::Enter => {
                if let Some(inp) = app.input.take() {
                    let text = inp.buffer.trim().to_string();
                    // Validation error to notify later (e.g. invalid JSON).
                    let mut err: Option<String> = None;
                    // The buffer's meaning depends on the input kind.
                    app.pending_action = match inp.kind {
                        InputKind::AclGrant { store, role } if !text.is_empty() => {
                            Some(PendingAction::AclGrant {
                                store,
                                role,
                                key_id: text,
                            })
                        }
                        InputKind::AclRevoke { store, role } if !text.is_empty() => {
                            Some(PendingAction::AclRevoke {
                                store,
                                role,
                                key_id: text,
                            })
                        }
                        InputKind::PeerConnect if !text.is_empty() => {
                            Some(PendingAction::PeerSync { node_id: text })
                        }
                        InputKind::BlobAddPath if !text.is_empty() => {
                            Some(PendingAction::BlobAdd { path: text })
                        }
                        InputKind::BlobExportPath { hash } if !text.is_empty() => {
                            Some(PendingAction::BlobExport { hash, path: text })
                        }
                        InputKind::KvEdit { store, key } => match validate_kv_value(&text) {
                            Ok(()) => Some(PendingAction::KvPut {
                                store,
                                key,
                                value: text,
                            }),
                            Err(m) => {
                                err = Some(m);
                                None
                            }
                        },
                        InputKind::KvCreate { store } => match text.split_once('=') {
                            Some((k, v)) if !k.trim().is_empty() => {
                                let key = k.trim().to_string();
                                let value = v.to_string();
                                match validate_kv_value(&value) {
                                    Ok(()) => Some(PendingAction::KvPut { store, key, value }),
                                    Err(m) => {
                                        err = Some(m);
                                        None
                                    }
                                }
                            }
                            _ => {
                                err = Some("Expected format: key=value".into());
                                None
                            }
                        },
                        InputKind::KvExportPath if !text.is_empty() => {
                            Some(PendingAction::KvExport { path: text })
                        }
                        InputKind::KeystoreGenerate if !text.is_empty() => {
                            Some(PendingAction::KeystoreGenerate { key_id: text })
                        }
                        // Logical clock filter (B5): applied directly to the state, no op.
                        InputKind::LogClockRange => {
                            match parse_clock_range(&text) {
                                Ok(range) => {
                                    app.log_clock_range = range;
                                    app.inspector_state.select(if app.inspector_len() > 0 {
                                        Some(0)
                                    } else {
                                        None
                                    });
                                }
                                Err(m) => err = Some(m),
                            }
                            None
                        }
                        // Append entry to the EventLog (G2.5): payload = typed text.
                        InputKind::EventLogAppend { store } if !text.is_empty() => {
                            Some(PendingAction::EventLogAppend { store, data: text })
                        }
                        // New document (G2.6): `id={json}` format (split on the 1st `=`).
                        InputKind::DocCreate { store } => match text.split_once('=') {
                            Some((id, json)) if !id.trim().is_empty() => {
                                let id = id.trim().to_string();
                                let json = json.to_string();
                                match validate_kv_value(&json) {
                                    Ok(()) => Some(PendingAction::DocPut { store, id, json }),
                                    Err(m) => {
                                        err = Some(m);
                                        None
                                    }
                                }
                            }
                            _ => {
                                err = Some("Expected format: id={\"field\":\"value\"}".into());
                                None
                            }
                        },
                        _ => None,
                    };
                    if let Some(m) = err {
                        app.notify_error(m);
                    }
                }
            }
            KeyCode::Esc => app.input = None,
            _ => {}
        }
        return;
    }

    // Active confirmation dialog: intercepts everything (s/y confirms, anything else cancels).
    if app.confirm.is_some() {
        match key.code {
            KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(c) = app.confirm.take() {
                    app.pending_action = Some(c.action);
                }
            }
            _ => app.confirm = None,
        }
        return;
    }

    match key.code {
        // Global quit
        KeyCode::Char('q') => {
            app.should_quit = true;
        }

        // Global navigation via function keys
        KeyCode::F(1) => {
            app.screen = Screen::Dashboard;
            app.screen_history.clear();
        }
        KeyCode::F(2) => {
            app.screen = Screen::NetworkTopology;
            app.screen_history.clear();
            app.screen_history.push(Screen::Dashboard);
            app.needs_fetch = true;
        }
        KeyCode::F(3) => {
            app.screen = Screen::ReplicationMonitor;
            app.screen_history.clear();
            app.screen_history.push(Screen::Dashboard);
            app.needs_fetch = true;
        }
        KeyCode::F(4) => {
            app.screen = Screen::AccessControlManager;
            app.screen_history.clear();
            app.screen_history.push(Screen::Dashboard);
            app.needs_fetch = true;
        }
        KeyCode::F(5) => {
            app.screen = Screen::KeystoreManager;
            app.screen_history.clear();
            app.screen_history.push(Screen::Dashboard);
            app.needs_fetch = true;
        }
        KeyCode::F(6) => {
            app.screen = Screen::BlobBrowser;
            app.screen_history.clear();
            app.screen_history.push(Screen::Dashboard);
            app.needs_fetch = true;
        }
        KeyCode::F(7) => {
            app.screen = Screen::EventBusExplorer;
            app.screen_history.clear();
            app.screen_history.push(Screen::Dashboard);
            app.needs_fetch = true;
        }

        // Contextual help for the current screen (G4.1) — in plain language.
        KeyCode::Char('?') => {
            let (title, body) = help_for_screen(&app.screen);
            app.help_modal = Some(InfoModal { title, body });
        }

        // Go back (or first clear an applied search / clock filter).
        KeyCode::Esc => {
            if app.search.is_some() || app.log_clock_range.is_some() {
                app.search = None;
                app.log_clock_range = None;
                app.inspector_state.select(if app.inspector_len() > 0 {
                    Some(0)
                } else {
                    None
                });
            } else {
                app.go_back();
            }
        }

        // Contextual actions
        _ => handle_screen_key(app, key),
    }
}

fn handle_screen_key(app: &mut App, key: KeyEvent) {
    match &app.screen {
        Screen::Dashboard => handle_dashboard_key(app, key),
        Screen::KeyValueInspector { .. }
        | Screen::DocumentInspector { .. }
        | Screen::EventLogInspector { .. }
        | Screen::ReplicationMonitor
        | Screen::BlobBrowser
        | Screen::AccessControlManager
        | Screen::KeystoreManager
        | Screen::NetworkTopology => handle_inspector_key(app, key),
        Screen::EventBusExplorer => handle_eventbus_key(app, key),
        // 'c' on the store detail connects/syncs with a peer (NodeId input).
        Screen::StoreDetail { .. } if key.code == KeyCode::Char('c') => {
            app.input = Some(InputPrompt {
                label: "NodeId to connect/sync: ".into(),
                buffer: String::new(),
                kind: InputKind::PeerConnect,
            });
        }
        _ => {}
    }
}

/// EventBus Explorer keys: scroll (turns follow off), follow (`f`), pause
/// (space), kind-filter cycle (`t`), search (`/`) and clear (`c`).
fn handle_eventbus_key(app: &mut App, key: KeyEvent) {
    let len = app.visible_events().len();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if len > 0 => {
            app.event_follow = false;
            let i = app.inspector_state.selected().unwrap_or(0);
            app.inspector_state.select(Some(i.saturating_sub(1)));
        }
        KeyCode::Down | KeyCode::Char('j') if len > 0 => {
            app.event_follow = false;
            let i = app.inspector_state.selected().unwrap_or(0);
            app.inspector_state.select(Some((i + 1).min(len - 1)));
        }
        KeyCode::PageUp if len > 0 => {
            app.event_follow = false;
            let i = app.inspector_state.selected().unwrap_or(0);
            app.inspector_state.select(Some(i.saturating_sub(10)));
        }
        KeyCode::PageDown if len > 0 => {
            app.event_follow = false;
            let i = app.inspector_state.selected().unwrap_or(0);
            app.inspector_state.select(Some((i + 10).min(len - 1)));
        }
        KeyCode::Char('f') => app.event_follow = !app.event_follow,
        KeyCode::Char(' ') => app.event_paused = !app.event_paused,
        KeyCode::Char('t') => {
            app.event_kind_filter = (app.event_kind_filter + 1) % EVENT_KINDS.len();
            app.inspector_state.select(None);
        }
        KeyCode::Char('/') => {
            app.event_follow = false;
            app.search = Some(String::new());
            app.searching = true;
        }
        KeyCode::Char('c') => {
            app.events.clear();
            app.inspector_state.select(None);
        }
        _ => {}
    }
}

/// Scroll and refresh shared by the inspection screens (scrollable list).
fn handle_inspector_key(app: &mut App, key: KeyEvent) {
    let len = app.inspector_len();
    match key.code {
        // EventLog paging (feature 2.1): scrolling past the oldest entry loads
        // the previous history block instead of wrapping to the end.
        KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp
            if matches!(app.screen, Screen::EventLogInspector { .. })
                && app.search.is_none()
                && app.log_clock_range.is_none()
                && app.log_has_more
                && app.inspector_state.selected() == Some(0) =>
        {
            app.needs_load_more = true;
        }
        KeyCode::Up | KeyCode::Char('k') if len > 0 => {
            let i = app.inspector_state.selected().unwrap_or(0);
            let new_i = if i == 0 { len - 1 } else { i - 1 };
            app.inspector_state.select(Some(new_i));
        }
        KeyCode::Down | KeyCode::Char('j') if len > 0 => {
            let i = app.inspector_state.selected().unwrap_or(0);
            let new_i = if i >= len - 1 { 0 } else { i + 1 };
            app.inspector_state.select(Some(new_i));
        }
        KeyCode::PageUp if len > 0 => {
            let i = app.inspector_state.selected().unwrap_or(0);
            app.inspector_state.select(Some(i.saturating_sub(10)));
        }
        KeyCode::PageDown if len > 0 => {
            let i = app.inspector_state.selected().unwrap_or(0);
            app.inspector_state.select(Some((i + 10).min(len - 1)));
        }
        // Refresh — except in the Keystore, where 'r' is rotate.
        KeyCode::Char('r') if app.screen != Screen::KeystoreManager => {
            app.needs_fetch = true;
        }
        // Delete the selected key (KeyValue inspector only) → confirmation.
        KeyCode::Char('d') if matches!(app.screen, Screen::KeyValueInspector { .. }) => {
            if let (Some(store), Some(key)) = (kv_store_name(app), kv_selected_key(app)) {
                app.confirm = Some(ConfirmPrompt {
                    message: format!("Delete key '{key}'? [y/N]"),
                    action: PendingAction::KvDelete { store, key },
                });
            }
        }
        // Detail of the selected key's value (KV inspector) → modal.
        KeyCode::Enter | KeyCode::Char('c')
            if matches!(app.screen, Screen::KeyValueInspector { .. }) =>
        {
            let detail = app.inspector_state.selected().and_then(|s| {
                app.visible_kv_entries()
                    .get(s)
                    .map(|e| (e.key.clone(), kv_detail(e)))
            });
            if let Some((key, body)) = detail {
                app.info_modal = Some(InfoModal {
                    title: format!("Key: {key}"),
                    body,
                });
            }
        }
        // Edit the selected key's value (KV inspector) → pre-filled input.
        KeyCode::Char('e') if matches!(app.screen, Screen::KeyValueInspector { .. }) => {
            let sel = app.inspector_state.selected().and_then(|s| {
                app.visible_kv_entries()
                    .get(s)
                    .map(|e| (e.key.clone(), e.value_utf8.clone()))
            });
            if let (Some(store), Some((key, value))) = (kv_store_name(app), sel) {
                app.input = Some(InputPrompt {
                    label: format!("New value for '{key}': "),
                    buffer: value,
                    kind: InputKind::KvEdit { store, key },
                });
            }
        }
        // Create a new key (KV inspector) → input in key=value format.
        KeyCode::Char('n') if matches!(app.screen, Screen::KeyValueInspector { .. }) => {
            if let Some(store) = kv_store_name(app) {
                app.input = Some(InputPrompt {
                    label: "New key (key=value format): ".into(),
                    buffer: String::new(),
                    kind: InputKind::KvCreate { store },
                });
            }
        }
        // Search keys (KV inspector).
        KeyCode::Char('/') if matches!(app.screen, Screen::KeyValueInspector { .. }) => {
            app.search = Some(String::new());
            app.searching = true;
        }
        // Search documents (Document inspector, B4).
        KeyCode::Char('/') if matches!(app.screen, Screen::DocumentInspector { .. }) => {
            app.search = Some(String::new());
            app.searching = true;
        }
        // Detail of the selected document (Document inspector) → async fetch + modal.
        KeyCode::Enter | KeyCode::Char('c')
            if matches!(app.screen, Screen::DocumentInspector { .. }) =>
        {
            if let Screen::DocumentInspector { store_name } = &app.screen {
                let id = app
                    .inspector_state
                    .selected()
                    .and_then(|s| app.visible_doc_entries().get(s).map(|d| d.id.clone()));
                if let Some(id) = id {
                    app.pending_action = Some(PendingAction::ShowDoc {
                        store: store_name.clone(),
                        id,
                    });
                }
            }
        }
        // Export all keys as JSON (KV inspector) → path input.
        KeyCode::Char('x') if matches!(app.screen, Screen::KeyValueInspector { .. }) => {
            app.input = Some(InputPrompt {
                label: "Output path (JSON): ".into(),
                buffer: String::new(),
                kind: InputKind::KvExportPath,
            });
        }
        // Grant/revoke 'write' on the selected controller (ACL manager only)
        // → opens the key_id input prompt.
        KeyCode::Char('g') | KeyCode::Char('x') if app.screen == Screen::AccessControlManager => {
            let store = app
                .inspector_state
                .selected()
                .and_then(|s| app.acls.get(s))
                .map(|a| a.store.clone());
            if let Some(store) = store {
                let grant = key.code == KeyCode::Char('g');
                app.input = Some(InputPrompt {
                    label: acl_role_label(grant, "write"),
                    buffer: String::new(),
                    kind: if grant {
                        InputKind::AclGrant {
                            store,
                            role: "write".into(),
                        }
                    } else {
                        InputKind::AclRevoke {
                            store,
                            role: "write".into(),
                        }
                    },
                });
            }
        }
        // Create a new controller (ACL manager only) → multi-step wizard.
        KeyCode::Char('n') if app.screen == Screen::AccessControlManager => {
            app.wizard = Some(ControllerWizard::new());
        }
        // Detail of the selected controller (ACL manager) → per-role modal.
        KeyCode::Enter if app.screen == Screen::AccessControlManager => {
            let body = app
                .inspector_state
                .selected()
                .and_then(|s| app.acls.get(s))
                .map(controller_detail);
            if let Some(body) = body {
                app.info_modal = Some(InfoModal {
                    title: "Controller Detail".into(),
                    body,
                });
            }
        }
        // Detail of the selected peer (replication monitor) → async fetch + modal.
        KeyCode::Enter if app.screen == Screen::ReplicationMonitor => {
            let node_id = app
                .inspector_state
                .selected()
                .and_then(|s| app.peers.get(s))
                .map(|p| p.node_id.clone());
            if let Some(node_id) = node_id {
                app.pending_action = Some(PendingAction::ShowPeer { node_id });
            }
        }
        // Force a sync with the selected peer (replication monitor only).
        KeyCode::Char('s') if app.screen == Screen::ReplicationMonitor => {
            let node_id = app
                .inspector_state
                .selected()
                .and_then(|s| app.peers.get(s))
                .map(|p| p.node_id.clone());
            if let Some(node_id) = node_id {
                app.pending_action = Some(PendingAction::PeerSync { node_id });
            }
        }
        // Connect/sync with an arbitrary NodeId → input prompt.
        KeyCode::Char('c') if app.screen == Screen::ReplicationMonitor => {
            app.input = Some(InputPrompt {
                label: "NodeId to connect/sync: ".into(),
                buffer: String::new(),
                kind: InputKind::PeerConnect,
            });
        }
        // Open search (EventLog inspector).
        KeyCode::Char('/') if matches!(app.screen, Screen::EventLogInspector { .. }) => {
            app.search = Some(String::new());
            app.searching = true;
        }
        // Filter by logical (Lamport) clock range on the EventLog (B5) → prompt.
        KeyCode::Char('t') if matches!(app.screen, Screen::EventLogInspector { .. }) => {
            app.input = Some(InputPrompt {
                label: "Logical clock range (min-max, empty clears): ".into(),
                buffer: String::new(),
                kind: InputKind::LogClockRange,
            });
        }
        // Append entry to the EventLog (G2.5) → payload prompt.
        KeyCode::Char('a') if matches!(app.screen, Screen::EventLogInspector { .. }) => {
            if let Screen::EventLogInspector { log_name } = &app.screen {
                app.input = Some(InputPrompt {
                    label: "New entry (payload): ".into(),
                    buffer: String::new(),
                    kind: InputKind::EventLogAppend {
                        store: log_name.clone(),
                    },
                });
            }
        }
        // New document (Document inspector, G2.6) → id={json} prompt.
        KeyCode::Char('n') if matches!(app.screen, Screen::DocumentInspector { .. }) => {
            if let Screen::DocumentInspector { store_name } = &app.screen {
                app.input = Some(InputPrompt {
                    label: "New doc (id={\"field\":\"value\"}): ".into(),
                    buffer: String::new(),
                    kind: InputKind::DocCreate {
                        store: store_name.clone(),
                    },
                });
            }
        }
        // Delete the selected document (Document inspector, G2.6) → confirmation.
        KeyCode::Char('d') if matches!(app.screen, Screen::DocumentInspector { .. }) => {
            if let Screen::DocumentInspector { store_name } = &app.screen {
                let id = app
                    .inspector_state
                    .selected()
                    .and_then(|s| app.visible_doc_entries().get(s).map(|d| d.id.clone()));
                if let Some(id) = id {
                    app.confirm = Some(ConfirmPrompt {
                        message: format!("Delete document '{id}'? [y/N]"),
                        action: PendingAction::DocDelete {
                            store: store_name.clone(),
                            id,
                        },
                    });
                }
            }
        }
        // View the log's CRDT heads (EventLog inspector) → async fetch + modal.
        KeyCode::Char('h') if matches!(app.screen, Screen::EventLogInspector { .. }) => {
            if let Screen::EventLogInspector { log_name } = &app.screen {
                app.pending_action = Some(PendingAction::ShowHeads {
                    store: log_name.clone(),
                });
            }
        }
        // Detail of the selected entry (EventLog inspector) → persistent modal.
        KeyCode::Enter | KeyCode::Char('c')
            if matches!(app.screen, Screen::EventLogInspector { .. }) =>
        {
            // The selection indexes the ALREADY-FILTERED list.
            let detail = app.inspector_state.selected().and_then(|s| {
                app.visible_log_entries()
                    .get(s)
                    .map(|e| (e.index, entry_detail(e)))
            });
            if let Some((index, body)) = detail {
                app.info_modal = Some(InfoModal {
                    title: format!("Entry #{index}"),
                    body,
                });
            }
        }
        // Detail of the selected blob (Blob Browser) → async fetch + modal.
        KeyCode::Enter | KeyCode::Char('c') if app.screen == Screen::BlobBrowser => {
            let hash = app
                .inspector_state
                .selected()
                .and_then(|s| app.blobs.get(s))
                .map(|b| b.hash.clone());
            if let Some(hash) = hash {
                app.pending_action = Some(PendingAction::ShowBlob { hash });
            }
        }
        // Toggle the blob sort criterion (Blob Browser): hash ↔ size.
        KeyCode::Char('s') if app.screen == Screen::BlobBrowser => {
            app.blob_sort = app.blob_sort.next();
            app.sort_blobs();
            app.inspector_state
                .select(if app.blobs.is_empty() { None } else { Some(0) });
        }
        // Add a blob from a file (Blob Browser) → path prompt.
        KeyCode::Char('a') if app.screen == Screen::BlobBrowser => {
            app.input = Some(InputPrompt {
                label: "File path to add: ".into(),
                buffer: String::new(),
                kind: InputKind::BlobAddPath,
            });
        }
        // Export the selected blob to a file (Blob Browser) → path prompt.
        KeyCode::Char('x') if app.screen == Screen::BlobBrowser => {
            let hash = app
                .inspector_state
                .selected()
                .and_then(|s| app.blobs.get(s))
                .map(|b| b.hash.clone());
            if let Some(hash) = hash {
                app.input = Some(InputPrompt {
                    label: "Output path to export: ".into(),
                    buffer: String::new(),
                    kind: InputKind::BlobExportPath { hash },
                });
            }
        }
        // Delete the selected blob (Blob Browser) → confirmation.
        KeyCode::Char('d') if app.screen == Screen::BlobBrowser => {
            let hash = app
                .inspector_state
                .selected()
                .and_then(|s| app.blobs.get(s))
                .map(|b| b.hash.clone());
            if let Some(hash) = hash {
                let short: String = hash.chars().take(12).collect();
                app.confirm = Some(ConfirmPrompt {
                    message: format!("Delete blob '{short}…'? (local copy only) [y/N]"),
                    action: PendingAction::BlobDelete { hash },
                });
            }
        }
        // Detail/export of the selected key (Keystore) → public key in a modal.
        KeyCode::Enter | KeyCode::Char('x') if app.screen == Screen::KeystoreManager => {
            let key_id = app
                .inspector_state
                .selected()
                .and_then(|s| app.keystore_keys.get(s))
                .cloned();
            if let Some(key_id) = key_id {
                app.pending_action = Some(PendingAction::ShowKey { key_id });
            }
        }
        // Generate a new key (Keystore) → ID prompt.
        KeyCode::Char('n') if app.screen == Screen::KeystoreManager => {
            app.input = Some(InputPrompt {
                label: "New key ID: ".into(),
                buffer: String::new(),
                kind: InputKind::KeystoreGenerate,
            });
        }
        // Rotate the selected key (Keystore) → confirmation (destructive).
        KeyCode::Char('r') if app.screen == Screen::KeystoreManager => {
            let key_id = app
                .inspector_state
                .selected()
                .and_then(|s| app.keystore_keys.get(s))
                .cloned();
            if let Some(key_id) = key_id {
                app.confirm = Some(ConfirmPrompt {
                    message: format!(
                        "Rotate '{key_id}'? Generates a new pair and OVERWRITES the old one [y/N]"
                    ),
                    action: PendingAction::KeystoreGenerate { key_id },
                });
            }
        }
        _ => {}
    }
}

/// Tests whether an entry matches the query (already lowercased) — substring
/// search in the payload, op, key, author and hash (feature 2.3).
fn log_matches(e: &LogEntry, query_lower: &str) -> bool {
    e.value_utf8.to_lowercase().contains(query_lower)
        || e.op.to_lowercase().contains(query_lower)
        || e.key
            .as_deref()
            .is_some_and(|k| k.to_lowercase().contains(query_lower))
        || e.identity
            .as_deref()
            .is_some_and(|i| i.to_lowercase().contains(query_lower))
        || e.hash.to_lowercase().contains(query_lower)
}

/// Splits `text` into spans highlighting the (case-insensitive) occurrences of `query`.
/// Only highlights when both are ASCII (to avoid invalid byte indices).
fn highlight_spans(text: &str, query: &str, base: Style, hl: Style) -> Vec<Span<'static>> {
    if query.is_empty() || !text.is_ascii() || !query.is_ascii() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let lower = text.to_lowercase();
    let ql = query.to_lowercase();
    let mut spans = Vec::new();
    let mut start = 0usize;
    while let Some(pos) = lower[start..].find(&ql) {
        let abs = start + pos;
        if abs > start {
            spans.push(Span::styled(text[start..abs].to_string(), base));
        }
        let end = abs + ql.len();
        spans.push(Span::styled(text[abs..end].to_string(), hl));
        start = end;
    }
    if start < text.len() {
        spans.push(Span::styled(text[start..].to_string(), base));
    }
    spans
}

/// Abbreviates a hash for display (first 12 chars).
fn short_hash(h: &str) -> String {
    h.chars().take(12).collect()
}

/// Formats the log's current CRDT heads for the modal (feature 2.4): count,
/// divergence indicator, the simplified diff between each head's branches, and
/// the merge timeline. Uses `entries` (with hash+next) to traverse the DAG.
fn heads_detail(heads: &[CrdtHead], entries: &[LogEntry]) -> String {
    use std::collections::{HashMap, HashSet};

    let mut s = String::new();
    match heads.len() {
        0 => {
            s.push_str("Heads: 0 (empty log)\n");
            return s;
        }
        1 => s.push_str("Heads: 1  ✓ converged (no divergence)\n\n"),
        n => s.push_str(&format!(
            "Heads: {n}  ⚠ DIVERGENCE — merge pending ({n} tips)\n\n"
        )),
    }
    for (i, h) in heads.iter().enumerate() {
        s.push_str(&format!(
            "[{}] clock {} @ {}",
            i + 1,
            h.clock_id,
            h.clock_time
        ));
        if let Some(id) = &h.identity {
            s.push_str(&format!("   author: {id}"));
        }
        s.push('\n');
        s.push_str(&format!("    hash: {}\n", h.hash));
    }

    // hash → entry index to traverse the DAG via the `next` pointers.
    let by_hash: HashMap<&str, &LogEntry> = entries.iter().map(|e| (e.hash.as_str(), e)).collect();

    // Set of ancestors (reachable via next, including the head itself).
    let ancestors = |start: &str| -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(h) = stack.pop() {
            if !seen.insert(h.clone()) {
                continue;
            }
            if let Some(e) = by_hash.get(h.as_str()) {
                for n in &e.next {
                    stack.push(n.clone());
                }
            }
        }
        seen
    };

    // Diff: entries exclusive to each head's branch (outside the common ancestor).
    if heads.len() > 1 {
        let anc: Vec<HashSet<String>> = heads.iter().map(|h| ancestors(&h.hash)).collect();
        let mut common = anc[0].clone();
        for a in &anc[1..] {
            common.retain(|x| a.contains(x));
        }
        s.push_str("\nDivergence (entries exclusive to each branch):\n");
        for (i, a) in anc.iter().enumerate() {
            let uniq: Vec<&String> = a.iter().filter(|x| !common.contains(*x)).collect();
            s.push_str(&format!("  head[{}]: {} entrie(s)\n", i + 1, uniq.len()));
            for h in uniq.iter().take(8) {
                let label = by_hash
                    .get(h.as_str())
                    .map(|e| format!("{} (op {}, clock {})", short_hash(h), e.op, e.clock_time))
                    .unwrap_or_else(|| short_hash(h));
                s.push_str(&format!("      · {label}\n"));
            }
            if uniq.len() > 8 {
                s.push_str(&format!("      … +{} more\n", uniq.len() - 8));
            }
        }
    }

    // Merge timeline: entries with more than one `next` (they merged multiple tips).
    let mut merges: Vec<&LogEntry> = entries.iter().filter(|e| e.next.len() > 1).collect();
    merges.sort_by_key(|e| e.clock_time);
    s.push('\n');
    if merges.is_empty() {
        s.push_str("Merges: none (linear log)\n");
    } else {
        s.push_str("Merges (entries that merged multiple tips):\n");
        let last = merges.len() - 1;
        for (i, e) in merges.iter().enumerate() {
            let connector = if i == last { "└─" } else { "├─" };
            s.push_str(&format!(
                "  {connector} clock {} @ {}  ({} parents: {})\n",
                e.clock_id,
                e.clock_time,
                e.next.len(),
                e.next
                    .iter()
                    .map(|h| short_hash(h))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    s
}

/// Formats a peer's detail for the modal (feature 5.2): full NodeID, status,
/// addresses, connection type/latency (if active) and recent syncs.
fn peer_detail(
    node_id: &str,
    peer: Option<&PeerSummary>,
    link: Option<&TopoLink>,
    events: &VecDeque<EventRecord>,
    shared_stores: &[(String, usize)],
) -> String {
    let mut s = String::new();
    s.push_str(&format!("NodeID: {node_id}\n"));
    match peer {
        Some(p) => {
            s.push_str(&format!(
                "Status: {}\n",
                if p.connected { "online" } else { "offline" }
            ));
            if p.addresses.is_empty() {
                s.push_str("Addresses: (none known)\n");
            } else {
                s.push_str(&format!("Addresses: {}\n", p.addresses.join(", ")));
            }
        }
        None => s.push_str("Status: unknown (not in the peer list)\n"),
    }
    match link {
        Some(l) => {
            // Real type (C1) when remote_info knows the peer; otherwise the inferred one.
            let kind = l
                .conn_type
                .clone()
                .map(|k| format!("{k} (real)"))
                .unwrap_or_else(|| format!("{} (inferred)", l.link_kind));
            s.push_str(&format!(
                "Connection: {} · {:.0}ms · {} ops · connected for {}s\n",
                kind, l.latency_ms, l.ops, l.connected_secs
            ));
        }
        None => s.push_str("Connection: no active connection\n"),
    }

    // Stores shared with this peer (B2, derived from the sync events).
    s.push_str("\nShared stores (observed):\n");
    if shared_stores.is_empty() {
        s.push_str("  (no sync with this peer in the buffer)\n");
    } else {
        for (store, syncs) in shared_stores.iter().take(10) {
            s.push_str(&format!("  {store}  · {syncs} syncs\n"));
        }
    }

    s.push_str("\nRecent syncs:\n");
    let recent: Vec<&EventRecord> = events
        .iter()
        .rev()
        .filter(|e| App::is_sync_event(&e.kind) && App::event_mentions_peer(e, node_id))
        .take(10)
        .collect();
    if recent.is_empty() {
        s.push_str("  (none in the event buffer)\n");
    } else {
        for e in recent {
            // Enrich with heads/duration when the event carries them (B1).
            let extra = match (e.heads_synced, e.duration_ms) {
                (Some(h), Some(d)) => format!(" · {h} heads in {d}ms"),
                (Some(h), None) => format!(" · {h} heads"),
                (None, Some(d)) => format!(" · {d}ms"),
                (None, None) => String::new(),
            };
            s.push_str(&format!("  {}  {}{}\n", e.ts, e.kind, extra));
        }
    }
    s
}

/// Formats an access controller's detail for the modal (feature 4.2): type and
/// all permissions grouped by role, with each one's key IDs.
fn controller_detail(acl: &AclSummary) -> String {
    let mut s = String::new();
    s.push_str(&format!("Store: {}\n", acl.store));
    s.push_str(&format!("Type:  {}\n", acl.controller_type));
    let total: usize = acl.roles.iter().map(|r| r.keys.len()).sum();
    s.push_str(&format!("Total authorized keys: {total}\n\n"));
    s.push_str("Permissions by role:\n");
    for r in &acl.roles {
        s.push_str(&format!("  {} ({}):\n", r.role, r.keys.len()));
        if r.keys.is_empty() {
            s.push_str("    (none)\n");
        } else {
            for k in &r.keys {
                s.push_str(&format!("    - {k}\n"));
            }
        }
    }
    s
}

/// Name of the current screen's KV store, if it is a KeyValue inspector.
fn kv_store_name(app: &App) -> Option<String> {
    if let Screen::KeyValueInspector { kv_name } = &app.screen {
        Some(kv_name.clone())
    } else {
        None
    }
}

/// Selected key in the KV inspector's visible list.
fn kv_selected_key(app: &App) -> Option<String> {
    app.inspector_state
        .selected()
        .and_then(|s| app.visible_kv_entries().get(s).map(|e| e.key.clone()))
}

/// Detail of a key's value for the modal (feature 3.2): full value, formatted as
/// indented JSON when applicable, plus size.
fn kv_detail(e: &KvEntry) -> String {
    let value = serde_json::from_str::<serde_json::Value>(&e.value_utf8)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| e.value_utf8.clone());
    format!(
        "Key: {}\nSize: {} bytes\n(changes replicate automatically to peers)\n\nValue:\n{}",
        e.key, e.size, value
    )
}

/// Validates a KV value: if it looks like JSON (starts with `{`/`[`), requires it
/// to be valid JSON; otherwise accepts it as plain text (feature 3.2).
fn validate_kv_value(v: &str) -> Result<(), String> {
    let t = v.trim_start();
    if t.starts_with('{') || t.starts_with('[') {
        serde_json::from_str::<serde_json::Value>(v)
            .map(|_| ())
            .map_err(|e| format!("invalid JSON: {e}"))
    } else {
        Ok(())
    }
}

/// Formats a blob's detail for the modal (feature 9.2): full hash, real size,
/// type (text/binary) and a preview of the first bytes.
fn blob_detail(hash: &str, c: &BlobContent) -> String {
    let mut s = String::new();
    s.push_str(&format!("Hash:  {hash}\n"));
    s.push_str(&format!("Size:  {} bytes\n", c.size));
    s.push_str(&format!(
        "Type:  {}\n\nPreview (first bytes):\n{}",
        if c.is_text { "text" } else { "binary" },
        if c.is_text {
            c.preview.clone()
        } else {
            "(binary content — not shown)".to_string()
        }
    ));
    s
}

/// Formats the full fields of an EventLog entry for the detail modal (feature
/// 2.2): CRDT metadata + payload as indented JSON when applicable.
fn entry_detail(e: &LogEntry) -> String {
    let payload = serde_json::from_str::<serde_json::Value>(&e.value_utf8)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| e.value_utf8.clone());

    let mut s = String::new();
    s.push_str(&format!("Op: {}   ", e.op));
    if let Some(k) = &e.key {
        s.push_str(&format!("Key: {k}"));
    }
    s.push('\n');
    s.push_str(&format!("Hash:  {}\n", e.hash));
    s.push_str(&format!("Log:   {}\n", e.log_id));
    if let Some(id) = &e.identity {
        s.push_str(&format!("Author: {id}\n"));
    }
    s.push_str(&format!("Clock: {} @ {}\n", e.clock_id, e.clock_time));
    if !e.next.is_empty() {
        s.push_str(&format!("Next:  {}\n", e.next.join(", ")));
    }
    s.push_str(&format!("Size:  {} bytes\n\nPayload:\n{}", e.size, payload));
    s
}

/// Navigates the controller creation wizard's steps. Esc goes back one step (or
/// cancels on the first); Enter advances / confirms.
fn handle_wizard_key(app: &mut App, key: KeyEvent) {
    let Some(mut w) = app.wizard.take() else {
        return;
    };
    match w.step {
        WizardStep::Type => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                w.type_idx = (w.type_idx + CTRL_TYPES.len() - 1) % CTRL_TYPES.len();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                w.type_idx = (w.type_idx + 1) % CTRL_TYPES.len();
            }
            KeyCode::Enter => {
                w.buffer = w.name.clone();
                w.step = WizardStep::Name;
            }
            KeyCode::Esc => return, // cancel (wizard was already taken)
            _ => {}
        },
        WizardStep::Name => match key.code {
            KeyCode::Char(c) => w.buffer.push(c),
            KeyCode::Backspace => {
                w.buffer.pop();
            }
            KeyCode::Enter => {
                w.name = w.buffer.clone();
                w.buffer = w.admin_keys.clone();
                w.step = WizardStep::Admin;
            }
            KeyCode::Esc => {
                w.buffer.clear();
                w.step = WizardStep::Type;
            }
            _ => {}
        },
        WizardStep::Admin => match key.code {
            KeyCode::Char(c) => w.buffer.push(c),
            KeyCode::Backspace => {
                w.buffer.pop();
            }
            KeyCode::Enter => {
                w.admin_keys = w.buffer.clone();
                w.buffer = w.write_keys.clone();
                w.step = WizardStep::Write;
            }
            KeyCode::Esc => {
                w.buffer = w.name.clone();
                w.step = WizardStep::Name;
            }
            _ => {}
        },
        WizardStep::Write => match key.code {
            KeyCode::Char(c) => w.buffer.push(c),
            KeyCode::Backspace => {
                w.buffer.pop();
            }
            KeyCode::Enter => {
                w.write_keys = w.buffer.clone();
                w.step = WizardStep::Confirm;
            }
            KeyCode::Esc => {
                w.buffer = w.admin_keys.clone();
                w.step = WizardStep::Admin;
            }
            _ => {}
        },
        WizardStep::Confirm => match key.code {
            KeyCode::Enter => {
                app.pending_action = Some(PendingAction::AclCreate {
                    controller_type: w.controller_type().to_string(),
                    name: w.name.clone(),
                    admin_keys: ControllerWizard::parse_keys(&w.admin_keys),
                    write_keys: ControllerWizard::parse_keys(&w.write_keys),
                });
                return; // wizard consumed
            }
            KeyCode::Esc => {
                w.buffer = w.write_keys.clone();
                w.step = WizardStep::Write;
            }
            _ => {}
        },
    }
    app.wizard = Some(w);
}

fn handle_dashboard_key(app: &mut App, key: KeyEvent) {
    let filtered_len = app.filtered_indices.len();

    match key.code {
        KeyCode::Up | KeyCode::Char('k') if filtered_len > 0 => {
            let i = app.store_list_state.selected().unwrap_or(0);
            let new_i = if i == 0 { filtered_len - 1 } else { i - 1 };
            app.store_list_state.select(Some(new_i));
        }
        KeyCode::Down | KeyCode::Char('j') if filtered_len > 0 => {
            let i = app.store_list_state.selected().unwrap_or(0);
            let new_i = if i >= filtered_len - 1 { 0 } else { i + 1 };
            app.store_list_state.select(Some(new_i));
        }
        KeyCode::Enter => {
            if let Some(store) = app.selected_store() {
                let screen = match store.store_type.as_str() {
                    "eventlog" => Screen::EventLogInspector {
                        log_name: store.address.clone(),
                    },
                    "keyvalue" => Screen::KeyValueInspector {
                        kv_name: store.address.clone(),
                    },
                    "document" => Screen::DocumentInspector {
                        store_name: store.address.clone(),
                    },
                    _ => Screen::StoreDetail {
                        store_address: store.address.clone(),
                    },
                };
                app.navigate_to(screen);
                app.needs_fetch = true;
            }
        }
        KeyCode::Tab => {
            app.store_filter = app.store_filter.next();
            app.apply_filter();
            // Select the first item when changing the filter
            if !app.filtered_indices.is_empty() {
                app.store_list_state.select(Some(0));
            }
        }
        KeyCode::Char('r') => {
            app.has_updates.store(true, Ordering::Relaxed);
        }
        // New store (G1.4) → opens the wizard.
        KeyCode::Char('n') => {
            app.store_wizard = Some(StoreWizard::new());
        }
        // Import a store from a DocTicket (G3.3) → wizard.
        KeyCode::Char('i') => {
            app.import_wizard = Some(ImportWizard::new());
        }
        // My identity (NodeId + addresses) for sharing (G3.1).
        KeyCode::Char('y') => {
            app.pending_action = Some(PendingAction::ShowIdentity);
        }
        // Audit trail of this session's actions (G5.2).
        KeyCode::Char('l') => {
            let body = if app.audit_log.is_empty() {
                "No administration action recorded in this session.\n\n\
                 Actions like create/drop store, grant/revoke access, rotate key\n\
                 and delete appear here with the time."
                    .to_string()
            } else {
                let mut s = String::from("This session's actions (most recent first):\n\n");
                for (ts, msg) in app.audit_log.iter() {
                    s.push_str(&format!("  {ts}  {msg}\n"));
                }
                s
            };
            app.help_modal = Some(InfoModal {
                title: "Audit trail".into(),
                body,
            });
        }
        // Share the selected store → generates tickets (G3.2).
        KeyCode::Char('s') => {
            if let Some(store) = app.selected_store() {
                app.pending_action = Some(PendingAction::ShareStore {
                    name: store.address.clone(),
                });
            }
        }
        // Close the selected store (releases the session; reopens on restart) (G2.1).
        KeyCode::Char('x') => {
            if let Some(store) = app.selected_store() {
                let name = store.db_name.clone();
                app.confirm = Some(ConfirmPrompt {
                    message: format!("Close '{name}'? (data kept; reopens on restart) [y/N]"),
                    action: PendingAction::StoreClose {
                        name: store.address.clone(),
                    },
                });
            }
        }
        // Drop the selected store (DELETES the data) (G2.2) → strong confirmation.
        KeyCode::Char('d') => {
            if let Some(store) = app.selected_store() {
                let name = store.db_name.clone();
                app.confirm = Some(ConfirmPrompt {
                    message: format!("⚠ DROP '{name}'? This DELETES the local data. [y/N]"),
                    action: PendingAction::StoreDrop {
                        name: store.address.clone(),
                    },
                });
            }
        }
        _ => {}
    }
}

/// "New store" wizard keys (G1.4): navigate Type → Name → Options → Confirm.
fn handle_store_wizard_key(app: &mut App, key: KeyEvent) {
    let Some(mut w) = app.store_wizard.take() else {
        return;
    };
    match w.step {
        StoreWizardStep::Kind => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                w.kind_idx = (w.kind_idx + STORE_KINDS.len() - 1) % STORE_KINDS.len();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                w.kind_idx = (w.kind_idx + 1) % STORE_KINDS.len();
            }
            KeyCode::Enter => w.step = StoreWizardStep::Name,
            KeyCode::Esc => return, // cancel
            _ => {}
        },
        StoreWizardStep::Name => match key.code {
            KeyCode::Char(c) => w.name.push(c),
            KeyCode::Backspace => {
                w.name.pop();
            }
            KeyCode::Enter if !w.name.trim().is_empty() => w.step = StoreWizardStep::Options,
            KeyCode::Esc => w.step = StoreWizardStep::Kind,
            _ => {}
        },
        StoreWizardStep::Options => match key.code {
            KeyCode::Up | KeyCode::Char('k') => w.opt_idx = (w.opt_idx + 2) % 3,
            KeyCode::Down | KeyCode::Char('j') => w.opt_idx = (w.opt_idx + 1) % 3,
            // Space/Enter on a toggle flips the selected option.
            KeyCode::Char(' ') => match w.opt_idx {
                0 => w.replicate = !w.replicate,
                1 => w.local_only = !w.local_only,
                _ => w.read_only = !w.read_only,
            },
            KeyCode::Enter => w.step = StoreWizardStep::Acl,
            KeyCode::Esc => w.step = StoreWizardStep::Name,
            _ => {}
        },
        // Optional address of an access controller to attach (G2.3).
        StoreWizardStep::Acl => match key.code {
            KeyCode::Char(c) => w.acl.push(c),
            KeyCode::Backspace => {
                w.acl.pop();
            }
            KeyCode::Enter => w.step = StoreWizardStep::Confirm,
            KeyCode::Esc => w.step = StoreWizardStep::Options,
            _ => {}
        },
        StoreWizardStep::Confirm => match key.code {
            KeyCode::Enter => {
                let acl_address = {
                    let a = w.acl.trim();
                    if a.is_empty() {
                        None
                    } else {
                        Some(a.to_string())
                    }
                };
                app.pending_action = Some(PendingAction::StoreCreate {
                    kind: w.kind().to_string(),
                    name: w.name.trim().to_string(),
                    replicate: w.replicate,
                    local_only: w.local_only,
                    read_only: w.read_only,
                    acl_address,
                });
                return; // wizard consumed
            }
            KeyCode::Esc => w.step = StoreWizardStep::Acl,
            _ => {}
        },
    }
    app.store_wizard = Some(w);
}

/// "Import store" wizard keys (G3.3): Type → Name → Ticket (paste).
fn handle_import_wizard_key(app: &mut App, key: KeyEvent) {
    let Some(mut w) = app.import_wizard.take() else {
        return;
    };
    match w.step {
        ImportStep::Kind => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                w.kind_idx = (w.kind_idx + IMPORT_KINDS.len() - 1) % IMPORT_KINDS.len();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                w.kind_idx = (w.kind_idx + 1) % IMPORT_KINDS.len();
            }
            KeyCode::Enter => w.step = ImportStep::Name,
            KeyCode::Esc => return, // cancel
            _ => {}
        },
        ImportStep::Name => match key.code {
            KeyCode::Char(c) => w.name.push(c),
            KeyCode::Backspace => {
                w.name.pop();
            }
            KeyCode::Enter if !w.name.trim().is_empty() => w.step = ImportStep::Ticket,
            KeyCode::Esc => w.step = ImportStep::Kind,
            _ => {}
        },
        ImportStep::Ticket => match key.code {
            // Tab toggles read-only; typing pastes the ticket.
            KeyCode::Tab => w.read_only = !w.read_only,
            KeyCode::Char(c) => w.ticket.push(c),
            KeyCode::Backspace => {
                w.ticket.pop();
            }
            KeyCode::Enter if !w.ticket.trim().is_empty() => {
                app.pending_action = Some(PendingAction::StoreImport {
                    kind: w.kind().to_string(),
                    name: w.name.trim().to_string(),
                    ticket: w.ticket.trim().to_string(),
                    read_only: w.read_only,
                });
                return; // wizard consumed
            }
            KeyCode::Esc => w.step = ImportStep::Name,
            _ => {}
        },
    }
    app.import_wizard = Some(w);
}

// ═══════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════

/// How the panel obtains its data: by opening the storage (owning the data-dir)
/// or by attaching to a live instance over the admin RPC.
enum LaunchMode {
    /// Opens `GuardianDB` directly (redb lock — only works if nobody else is using
    /// the data-dir).
    Embedded(PathBuf),
    /// Connects to `guardian-sentinel-server` over a socket; doesn't touch the storage.
    /// `token` authenticates the connection when the server requires it.
    Connect { addr: String, token: Option<String> },
}

fn parse_args() -> LaunchMode {
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = PathBuf::from("./guardian_admin_data");
    let mut connect: Option<String> = None;
    let mut token: Option<String> = std::env::var("GUARDIAN_ADMIN_TOKEN").ok();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--connect" if i + 1 < args.len() => {
                connect = Some(args[i + 1].clone());
                i += 2;
            }
            "--data-dir" if i + 1 < args.len() => {
                data_dir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--token" if i + 1 < args.len() => {
                token = Some(args[i + 1].clone());
                i += 2;
            }
            _ => i += 1,
        }
    }

    match connect {
        Some(addr) => LaunchMode::Connect { addr, token },
        None => LaunchMode::Embedded(data_dir),
    }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mode = parse_args();
    let log_buffer = LogBuffer::new();

    // Set up tracing to capture logs in the TUI
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "warn,guardian_db=info,iroh=warn".to_string()),
        )
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_writer(log_buffer.clone())
        .with_ansi(false)
        .compact()
        .init();

    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal, log_buffer, mode).await;
    ratatui::restore();

    if let Err(ref e) = result {
        eprintln!("Error: {e}");
    }

    result
}

/// Heuristic to recognize an open failure caused by an exclusive file lock. Both
/// GuardianDB's redb stores (keystore, cache) and Iroh's internal redb stores
/// (blobs/docs) hold a per-process lock; a second process trying to open the same
/// `data-dir` fails with one of these messages instead of corrupting the data.
fn is_lock_error(raw: &str) -> bool {
    let m = raw.to_lowercase();
    m.contains("already open")
        || m.contains("acquire lock")
        || m.contains("being used by another process") // Windows (os error 32)
        || m.contains("os error 32")
        || m.contains("resource temporarily unavailable") // flock EWOULDBLOCK (unix)
        || m.contains("os error 11")
        || m.contains("os error 35")
}

/// Builds the message shown on the `ConnectionFailed` screen, with specific
/// guidance when the cause is a `data-dir` already in use.
fn connection_error_message(data_dir: &std::path::Path, raw: &str) -> String {
    if is_lock_error(raw) {
        format!(
            "The data directory is already in use by another process.\n\n\
             data-dir: {}\n\n\
             GuardianDB persists in redb, which holds an exclusive file lock: \
             only one process can open the same data-dir at a time (the same applies \
             to Iroh's internal stores). The data was NOT corrupted — the open \
             was simply refused.\n\n\
             How to resolve:\n\
             • stop the instance already using this directory, or\n\
             • point the panel at another directory with --data-dir <path>.\n\n\
             Technical detail: {}",
            data_dir.display(),
            raw
        )
    } else {
        format!(
            "Failed to initialize GuardianDB.\n\n\
             data-dir: {}\n\n\
             Technical detail: {}",
            data_dir.display(),
            raw
        )
    }
}

/// Minimal error-display loop: keeps the TUI alive showing the failure screen
/// until the user quits with 'q'/Esc. Returns `Ok` so `main` doesn't print the
/// error over the already-restored terminal.
fn run_error_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &App,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if event::poll(std::time::Duration::from_millis(150))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
}

async fn run_app(
    terminal: &mut ratatui::DefaultTerminal,
    log_buffer: LogBuffer,
    mode: LaunchMode,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let source_label = match &mode {
        LaunchMode::Embedded(dir) => format!("dir: {}", dir.display()),
        LaunchMode::Connect { addr, .. } => format!("rpc: {addr}"),
    };
    let mut app = App::new(log_buffer, source_label);

    // Detect the graphics protocol ALREADY on the alternate screen (ratatui-image
    // requirement). If there's a protocol (Sixel/Kitty/iTerm2), prepare the
    // protocols for crisp rendering; otherwise the render falls back to quadrant blocks.
    {
        use ratatui_image::picker::ProtocolType;
        let picker = build_picker();
        app.graphics = picker.protocol_type() != ProtocolType::Halfblocks;
        if app.graphics {
            *app.logo_proto.borrow_mut() =
                app.logo_img.clone().map(|i| picker.new_resize_protocol(i));
            *app.header_proto.borrow_mut() = app
                .header_img
                .clone()
                .map(|i| picker.new_resize_protocol(i));
        }
    }

    // Render the connection screen (splash) and keep it visible for 3s before
    // opening the DB, to give time to see the logo.
    terminal.draw(|f| ui(f, &app))?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Establish the data source. `_db` keeps the storage open (and the listeners
    // alive) in embedded mode; in RPC mode there's no local GuardianDB.
    let (source, _db): (Arc<dyn AdminSource>, Option<Arc<GuardianDB>>) = match mode {
        LaunchMode::Embedded(data_dir) => {
            // Owner of the data-dir (redb lock). Shared setup with the server bin.
            let (db, client) = match guardian_db::sentinel::open_owned(&data_dir).await {
                Ok(pair) => pair,
                Err(e) => {
                    app.screen = Screen::ConnectionFailed {
                        message: connection_error_message(&data_dir, &e.to_string()),
                    };
                    return run_error_loop(terminal, &app);
                }
            };

            let ctx = AdminContext::with_data_dir(db.clone(), client, data_dir);
            // Reopen stores created in earlier sessions (G1) so they reappear.
            ctx.reopen_stores().await;
            (Arc::new(EmbeddedSource::new(ctx)), Some(db))
        }
        LaunchMode::Connect { addr, token } => {
            let client = match AdminClient::connect(&addr).await {
                Ok(c) => c,
                Err(e) => {
                    app.screen = Screen::ConnectionFailed {
                        message: format!(
                            "Could not connect to the admin RPC.\n\n\
                             address: {addr}\n\n\
                             Check that guardian-sentinel-server is running and \
                             listening on that address.\n\n\
                             Technical detail: {e}"
                        ),
                    };
                    return run_error_loop(terminal, &app);
                }
            };
            // Authenticate if a token was provided (a server with --token requires it).
            if let Some(token) = &token
                && let Err(e) = client.authenticate(token).await
            {
                app.screen = Screen::ConnectionFailed {
                    message: format!(
                        "Authentication with the admin RPC failed.\n\n\
                         address: {addr}\n\n\
                         Check the token (--token / GUARDIAN_ADMIN_TOKEN).\n\n\
                         Technical detail: {}",
                        e.code
                    ),
                };
                return run_error_loop(terminal, &app);
            }
            (Arc::new(client), None)
        }
    };

    // Reactive refresh via the seam (R2): each event sets `has_updates` and feeds
    // the EventBus Explorer buffer (R8). Works in both modes — local EventBus
    // (embedded) or RPC stream (--connect).
    {
        let flag = app.has_updates.clone();
        let incoming = app.incoming.clone();
        let source = source.clone();
        tokio::spawn(async move {
            if let Ok(mut stream) = source.events_subscribe().await {
                while let Some(ev) = stream.next().await {
                    flag.store(true, Ordering::Relaxed);
                    let rec = EventRecord {
                        ts: chrono::Local::now().format("%H:%M:%S").to_string(),
                        at: Instant::now(),
                        kind: ev.kind,
                        detail: ev.detail,
                        store: ev.store,
                        peer: ev.peer,
                        heads_synced: ev.heads_synced,
                        duration_ms: ev.duration_ms,
                    };
                    if let Ok(mut inc) = incoming.lock() {
                        inc.push_back(rec);
                        while inc.len() > 2000 {
                            inc.pop_front();
                        }
                    }
                }
            }
        });
    }

    // Node id via the seam — works in both modes.
    if let Ok(info) = source.node_info().await {
        app.node_id = info.node_id;
    }

    // Transition to the Dashboard
    app.screen = Screen::Dashboard;
    if app.node_id.is_empty() {
        app.notify_success("Connected!");
    } else {
        app.notify_success(format!(
            "Connected! Node: {}…",
            &app.node_id[..app.node_id.len().min(12)]
        ));
    }

    // Initial refresh
    app.refresh_stores(source.as_ref()).await;

    // Onboarding (G4.3): in an empty data-dir (no stores), show a quick guide.
    if app.stores.is_empty() {
        app.help_modal = Some(InfoModal {
            title: "Welcome to Guardian-DB".into(),
            body: "This panel manages Guardian-DB — without writing code.\n\
                \n\
                Get started in 3 steps:\n\
                • 1. Press n to create a store (choose EventLog, KeyValue or Document).\n\
                • 2. Open the store with Enter and add data (n/a depending on the type).\n\
                • 3. Press s to share (generates a ticket) or y to see your NodeId;\n\
                     a friend uses i to import the ticket and you sync.\n\
                \n\
                Tips:\n\
                • ? opens the current screen's help at any time.\n\
                • The available keys always appear in the footer.\n\
                • What you create reopens on its own when you restart the panel."
                .into(),
        });
    }

    // ─── Main event loop ─────────────────────────────
    loop {
        terminal.draw(|f| ui(f, &app))?;

        // Poll with a 100ms timeout
        if event::poll(std::time::Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            handle_key(&mut app, key);
        }

        // Tick: clear expired notifications
        app.tick_notifications();

        // Drain the captured events into the display buffer (respects pause).
        app.drain_events();
        // In follow mode on the EventBus, keep the selection on the most recent event.
        if app.screen == Screen::EventBusExplorer && app.event_follow {
            let n = app.visible_events().len();
            app.inspector_state
                .select(if n > 0 { Some(n - 1) } else { None });
        }

        // Run the confirmed action (e.g. delete key) before reloading.
        if app.pending_action.is_some() {
            app.run_pending_action(source.as_ref()).await;
        }

        // Load the inspection screen's data on entering it or on refresh request.
        if app.needs_fetch {
            app.load_screen(source.as_ref()).await;
        }

        // Page one more block of EventLog history when requested (2.1).
        if app.needs_load_more {
            app.load_more_log_entries(source.as_ref()).await;
        }

        // Reactive refresh (the seam's event stream) + a slow periodic fallback
        // as a safety net, in case some event is missed.
        let periodic = app.last_refresh.elapsed() >= std::time::Duration::from_secs(5);
        if app.has_updates.swap(false, Ordering::Relaxed) || periodic {
            app.refresh_stores(source.as_ref()).await;
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}
