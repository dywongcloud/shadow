//! Per-node content-addressed blob store for shadw drive file bytes
//! (bn-shadw-drive-v1). One `iroh_blobs::store::fs::FsStore` per node, at
//! `$HIVE_DATA/drive-blobs`, addressed by BLAKE3 [`Hash`] — the same hash
//! `drive_nodes.content_hash` stores as hex.
//!
//! This is a LOCAL on-disk store only: no `iroh::Endpoint`, no ALPN acceptor,
//! no second network listener. Cross-node byte fetch-on-miss goes over the
//! existing admin/gossip proxy transport (`drive_api.rs`), the same
//! owner-routed shape Hrana/SQLite and `browser_artifacts.rs` already use for
//! node-local bytes behind a round-robin ingress — not iroh-blobs' own GET
//! wire protocol, which would need its own `Router`/`Endpoint` plumbing on
//! top of hive-p2p's existing mesh endpoint, well beyond what v1 needs.

use iroh_blobs::store::fs::FsStore;
use iroh_blobs::Hash;
use tokio::sync::OnceCell;

static STORE: OnceCell<FsStore> = OnceCell::const_new();

async fn store() -> anyhow::Result<&'static FsStore> {
    STORE
        .get_or_try_init(|| async {
            let dir = crate::persist::data_dir().join("drive-blobs");
            tokio::fs::create_dir_all(&dir).await?;
            FsStore::load(&dir)
                .await
                .map_err(|e| anyhow::anyhow!("drive blob store load: {e}"))
        })
        .await
}

/// Write `bytes` to the local store, returning its BLAKE3 hash (hex) — the
/// exact value callers persist as `drive_nodes.content_hash`.
pub(crate) async fn add_bytes(bytes: bytes::Bytes) -> anyhow::Result<String> {
    let store = store().await?;
    let outcome = store
        .add_bytes(bytes)
        .await
        .map_err(|e| anyhow::anyhow!("drive blob add: {e}"))?;
    Ok(outcome.hash.to_hex())
}

/// Read the full bytes for `hash` from the local store, bounded by
/// `max_bytes` (the caller's own recorded `size_bytes`, plus headroom for a
/// corrupted/mismatched entry) so a wedged or oversized local file can never
/// be read into memory unbounded.
pub(crate) async fn read_local(hash: &str, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let h: Hash = hash
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid content hash"))?;
    let store = store().await?;
    if !store.has(h).await.unwrap_or(false) {
        anyhow::bail!("blob not present locally");
    }
    let mut buf = Vec::new();
    let cap = max_bytes.saturating_add(64 * 1024);
    let mut limited = store.reader(h).take(cap);
    limited.read_to_end(&mut buf).await?;
    if buf.len() as u64 > max_bytes {
        anyhow::bail!("local blob larger than the recorded size");
    }
    Ok(buf)
}

/// Whether `bytes` genuinely hashes to `hash_hex` — the last check before ANY
/// byte source (local disk, or a proxied peer reply) is served under this
/// hash's identity, mirroring `browser_artifacts::verify_bytes`'s posture: a
/// peer that answers with wrong-but-same-length bytes must never be trusted
/// merely because the length matched.
pub(crate) fn matches(bytes: &[u8], hash_hex: &str) -> bool {
    Hash::new(bytes).to_hex() == hash_hex
}
