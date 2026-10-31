//! SHADW signed logs on a node: a store of Hypercore cores (through the
//! vendored `shadw-core`) and the pull protocol that replicates them over the
//! mesh on stream mode `0x06` ([`hive_p2p::STREAM_CORE`]). SHADW chain plan,
//! Annex IV, phase L3.
//!
//! Wire, after the mode byte (integers big-endian):
//!
//! ```text
//! reader → writer   [32B discovery key][32B capability]
//! writer → reader   [8B MAGIC][1B status][u64 available length]
//! then, repeated:   reader → [u64 from][u32 count]
//!                   writer → [u32 len][shadw proof bundle JSON]  (len 0 = nothing to send)
//! the reader finishes its send side when it is done.
//! ```
//!
//! **Capability.** `SHA-256(CAP_LABEL ‖ key ‖ EKM)`, where EKM is 32 bytes of TLS
//! keying material exported from this QUIC connection with `CAP_LABEL` as the
//! label and the core key as the context. Only a peer that knows the core key
//! can ask for it, and a capability recorded on one connection is useless on
//! any other. An unknown core and a wrong capability get the same refusal, so a
//! peer cannot probe which cores a node holds.
//!
//! **Trust.** Readers believe nothing the writer says: every bundle is verified by
//! `shadw-core` against the core's pinned key and signed Merkle head before it
//! is stored, and a bundle that would roll history back is refused. The mesh
//! trust set only limits who may open the stream at all.
//!
//! **Paths.** A core's directory is the lowercase hex of its 32-byte public key,
//! produced here from bytes, never from a peer- or tenant-supplied string.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use hive_p2p::{CoreHandler, CoreStream};
use sha2::{Digest, Sha256};
use shadw_core::{Core, CoreInfo, ReplicationBundle, MAX_BUNDLE_BYTES};
use tokio::sync::{Mutex, RwLock};

/// First bytes of every writer reply. An old node misparses `0x06` as a tunnel
/// session; the magic lets the reader tell that apart from a real refusal.
pub const MAGIC: &[u8; 8] = b"HVCORE01";
/// Most blocks one request may ask for; bounds every reply frame together with
/// `shadw_core::MAX_BUNDLE_BYTES`. The writer waits (up to `IO_BUDGET`) while the
/// reader verifies and durably stores each bundle: measured at ~12 ms per block
/// on SSD and ~270 ms on a spinning disk, so 64 blocks stays well inside the
/// budget on both, where 256 overran it on the disk.
pub const MAX_BLOCKS_PER_REQUEST: u32 = 64;
const CAP_LABEL: &[u8] = b"shadw/core/cap/0";
const DISCOVERY_LABEL: &[u8] = b"shadw/core/discovery/0";
const STATUS_OK: u8 = 0;
const STATUS_REFUSED: u8 = 1;
/// Per-read budget, the same order as the mesh's gossip idle budget.
const IO_BUDGET: Duration = Duration::from_secs(45);

/// Hash of a core key that is safe to send and announce.
pub fn discovery_key(key: &[u8; 32]) -> [u8; 32] {
    Sha256::new().chain_update(DISCOVERY_LABEL).chain_update(key).finalize().into()
}

fn capability(stream: &CoreStream, key: &[u8; 32]) -> Result<[u8; 32]> {
    let mut ekm = [0u8; 32];
    stream
        .conn
        .export_keying_material(&mut ekm, CAP_LABEL, key)
        .map_err(|e| anyhow!("TLS keying-material export failed: {e:?}"))?;
    Ok(Sha256::new().chain_update(CAP_LABEL).chain_update(key).chain_update(ekm).finalize().into())
}

async fn timed<T, E>(what: &str, f: impl std::future::Future<Output = std::result::Result<T, E>>) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::time::timeout(IO_BUDGET, f)
        .await
        .map_err(|_| anyhow!("{what}: no data within {}s", IO_BUDGET.as_secs()))?
        .with_context(|| what.to_string())
}

/// Result of one [`CoreStore::pull`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullReport {
    /// Length the remote node offered.
    pub remote_length: u64,
    /// Blocks verified and stored by this pull.
    pub imported_blocks: u64,
    /// Local length afterwards.
    pub length: u64,
}

struct Entry {
    key: [u8; 32],
    core: Arc<Mutex<Core>>,
}

/// Every core this node holds, keyed by discovery key.
pub struct CoreStore {
    root: PathBuf,
    cores: RwLock<HashMap<[u8; 32], Entry>>,
}

impl CoreStore {
    /// Open (creating if needed) the store at `root` and every core in it. A core
    /// that fails to open is logged and skipped, never fatal to the node.
    pub async fn open(root: &Path) -> Result<Arc<Self>> {
        std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        let mut cores = HashMap::new();
        for dir in std::fs::read_dir(root)? {
            let path = dir?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            match Core::open(&path).await {
                Ok(core) => {
                    let key = core.public_key();
                    cores.insert(discovery_key(&key), Entry { key, core: Arc::new(Mutex::new(core)) });
                }
                Err(e) => tracing::warn!(core = name, error = %e, "hive-cores: skipped a core that failed to open"),
            }
        }
        tracing::info!(root = %root.display(), cores = cores.len(), "hive-cores: store open");
        Ok(Arc::new(Self { root: root.to_path_buf(), cores: RwLock::new(cores) }))
    }

    fn dir_for(&self, key: &[u8; 32]) -> PathBuf {
        self.root.join(hex::encode(key))
    }

    /// Create a new core this node writes. Returns its public key.
    pub async fn create_writer(&self) -> Result<[u8; 32]> {
        // shadw-core picks the key at creation, so build under a staging name and
        // move it to its key-named directory once the store lock is released.
        let staging = self.root.join(format!(".new-{}", hex::encode(rand_suffix())));
        let key = Core::create(&staging).await?.public_key();
        std::fs::rename(&staging, self.dir_for(&key))?;
        let core = Core::open(&self.dir_for(&key)).await?;
        self.cores.write().await.insert(discovery_key(&key), Entry { key, core: Arc::new(Mutex::new(core)) });
        Ok(key)
    }

    /// Start following a core written elsewhere: an empty read-only replica
    /// pinned to `key`, filled by [`CoreStore::pull`]. Idempotent.
    pub async fn follow(&self, key: [u8; 32]) -> Result<()> {
        let disc = discovery_key(&key);
        if self.cores.read().await.contains_key(&disc) {
            return Ok(());
        }
        let core = Core::create_replica(&self.dir_for(&key), key).await?;
        self.cores.write().await.entry(disc).or_insert(Entry { key, core: Arc::new(Mutex::new(core)) });
        Ok(())
    }

    async fn entry(&self, key: &[u8; 32]) -> Result<Arc<Mutex<Core>>> {
        self.cores
            .read()
            .await
            .get(&discovery_key(key))
            .map(|e| e.core.clone())
            .ok_or_else(|| anyhow!("core {} is not held here", hex::encode(key)))
    }

    pub async fn append(&self, key: &[u8; 32], data: &[u8]) -> Result<u64> {
        Ok(self.entry(key).await?.lock().await.append(data).await?)
    }

    pub async fn get(&self, key: &[u8; 32], index: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.entry(key).await?.lock().await.get(index).await?)
    }

    pub async fn info(&self, key: &[u8; 32]) -> Result<CoreInfo> {
        Ok(self.entry(key).await?.lock().await.info())
    }

    pub async fn keys(&self) -> Vec<[u8; 32]> {
        self.cores.read().await.values().map(|e| e.key).collect()
    }

    /// The [`CoreHandler`] to pass to `hive_p2p::serve_tunnels_full`.
    pub fn handler(self: &Arc<Self>) -> CoreHandler {
        let store = self.clone();
        Arc::new(move |stream: CoreStream| {
            let store = store.clone();
            Box::pin(async move {
                let peer = stream.remote_id.clone();
                if let Err(e) = store.serve(stream).await {
                    tracing::debug!(peer = %peer, error = %e, "hive-cores: core stream ended with an error");
                }
            })
        })
    }

    /// Writer side of one stream.
    pub async fn serve(&self, mut s: CoreStream) -> Result<()> {
        let mut hello = [0u8; 64];
        timed("read request", s.recv.read_exact(&mut hello)).await?;
        let disc: [u8; 32] = hello[..32].try_into().unwrap();
        let found = self.cores.read().await.get(&disc).map(|e| (e.key, e.core.clone()));
        let admitted = match &found {
            Some((key, _)) => capability(&s, key)?.as_slice() == &hello[32..],
            None => false,
        };
        let mut head = MAGIC.to_vec();
        let Some((_, core)) = found.filter(|_| admitted) else {
            head.push(STATUS_REFUSED);
            s.send.write_all(&head).await?;
            s.send.finish()?;
            bail!("refused: unknown core or bad capability");
        };
        let available = core.lock().await.info().contiguous_length;
        head.push(STATUS_OK);
        head.extend(available.to_be_bytes());
        s.send.write_all(&head).await?;

        let mut ask = [0u8; 12];
        loop {
            match tokio::time::timeout(IO_BUDGET, s.recv.read_exact(&mut ask)).await {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break, // reader finished
                Err(_) => bail!("reader idle for {}s", IO_BUDGET.as_secs()),
            }
            let from = u64::from_be_bytes(ask[..8].try_into().unwrap());
            let count = u32::from_be_bytes(ask[8..].try_into().unwrap()).min(MAX_BLOCKS_PER_REQUEST) as u64;
            let end = from.saturating_add(count).min(available);
            if from >= end {
                s.send.write_all(&0u32.to_be_bytes()).await?;
                continue;
            }
            let indices: Vec<u64> = (from..end).collect();
            let bundle = core.lock().await.export_bundle(&indices).await?;
            let bytes = serde_json::to_vec(&bundle)?;
            s.send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
            s.send.write_all(&bytes).await?;
        }
        s.send.finish()?;
        Ok(())
    }

    /// Reader side: bring the local replica of `key` up to what the peer behind
    /// `s` offers. Call [`CoreStore::follow`] first.
    pub async fn pull(&self, mut s: CoreStream, key: &[u8; 32]) -> Result<PullReport> {
        let core = self.entry(key).await?;
        if core.lock().await.info().writable {
            bail!("this node writes core {}; it is never pulled", hex::encode(key));
        }
        let mut hello = discovery_key(key).to_vec();
        hello.extend(capability(&s, key)?);
        s.send.write_all(&hello).await?;

        let mut magic = [0u8; 8];
        timed("read reply", s.recv.read_exact(&mut magic)).await?;
        if &magic != MAGIC {
            bail!("peer does not speak the core protocol (an older node?)");
        }
        let mut status = [0u8; 1];
        timed("read status", s.recv.read_exact(&mut status)).await?;
        if status[0] != STATUS_OK {
            bail!("peer refused core {}", hex::encode(key));
        }
        let mut len = [0u8; 8];
        timed("read length", s.recv.read_exact(&mut len)).await?;
        let remote_length = u64::from_be_bytes(len);

        let mut imported = 0;
        loop {
            let have = core.lock().await.info().contiguous_length;
            if have >= remote_length {
                break;
            }
            let mut ask = have.to_be_bytes().to_vec();
            ask.extend(MAX_BLOCKS_PER_REQUEST.to_be_bytes());
            s.send.write_all(&ask).await?;
            let mut n = [0u8; 4];
            timed("read frame length", s.recv.read_exact(&mut n)).await?;
            let n = u32::from_be_bytes(n) as usize;
            if n == 0 || n > MAX_BUNDLE_BYTES {
                bail!("peer sent a frame of {n} bytes at block {have}");
            }
            let mut frame = vec![0u8; n];
            timed("read frame", s.recv.read_exact(&mut frame)).await?;
            let bundle: ReplicationBundle = serde_json::from_slice(&frame).context("bundle is not valid JSON")?;
            let report = core.lock().await.import_bundle(&bundle).await.context("bundle failed verification")?;
            if report.contiguous_length <= have {
                bail!("no progress past block {have}");
            }
            imported += report.imported_blocks;
        }
        s.send.finish()?;
        let length = core.lock().await.info().length;
        Ok(PullReport { remote_length, imported_blocks: imported, length })
    }
}

fn rand_suffix() -> [u8; 8] {
    // Uniqueness only (a staging directory name), not secrecy.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let mut out = [0u8; 8];
    out.copy_from_slice(&Sha256::digest(format!("{nanos}-{}", std::process::id()).as_bytes())[..8]);
    out
}
