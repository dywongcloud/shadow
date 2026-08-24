//! API for iroh-docs replicas

// Names and concepts are roughly based on Willows design at the moment:
//
// https://hackmd.io/DTtck8QOQm6tZaQBBtTf7w
//
// This is going to change!

use std::{
    cmp::Ordering,
    fmt::Debug,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use bytes::{Bytes, BytesMut};
// `iroh::Signature` is a newtype wrapper around `ed25519_dalek::Signature` with a
// hand-written, wire-stable `serialize_tuple` serde impl (the same impl every other
// iroh crate uses for handshake / discovery payloads). Embedding it inside
// `EntrySignature` — rather than the raw dalek type — keeps the on-wire
// `SignedEntry` format independent of upstream `ed25519` serde changes.
use iroh::{KeyParsingError, Signature, SignatureError};
use iroh_blobs::Hash;
use n0_future::{
    time::{Duration, SystemTime},
    IterExt,
};
use serde::{Deserialize, Serialize};

pub use crate::heads::AuthorHeads;
use crate::{
    keys::{Author, AuthorId, AuthorPublicKey, NamespaceId, NamespacePublicKey, NamespaceSecret},
    ranger::{self, Fingerprint, InsertOutcome, RangeEntry, RangeKey, RangeValue, Store},
    store::{self, fs::StoreInstance, DownloadPolicyStore, PublicKeyStore},
};

/// Protocol message for the set reconciliation protocol.
///
/// Can be serialized to bytes with [serde] to transfer between peers.
pub type ProtocolMessage = crate::ranger::Message<SignedEntry>;

/// Byte representation of an iroh `EndpointId`.
// TODO: Consider `iroh::EndpointId` instead of raw bytes (`iroh` re-exports it from `iroh-base`).
pub type PeerIdBytes = [u8; 32];

/// Max time in the future from our wall clock time that we accept entries for.
/// Value is 10 minutes.
pub const MAX_TIMESTAMP_FUTURE_SHIFT: u64 = 10 * 60 * Duration::from_secs(1).as_micros() as u64;

/// Callback that may be set on a replica to determine the availability status for a content hash.
pub type ContentStatusCallback =
    Arc<dyn Fn(Hash) -> n0_future::boxed::BoxFuture<ContentStatus> + Send + Sync + 'static>;

/// Event emitted by sync when entries are added.
#[derive(derive_more::Debug, Clone)]
pub enum Event {
    /// A local entry has been added.
    LocalInsert {
        /// Document in which the entry was inserted.
        namespace: NamespaceId,
        /// Inserted entry.
        entry: SignedEntry,
    },
    /// A remote entry has been added.
    RemoteInsert {
        /// Document in which the entry was inserted.
        namespace: NamespaceId,
        /// Inserted entry.
        entry: SignedEntry,
        /// Peer that provided the inserted entry.
        /// Debug matches [`iroh::PublicKey::fmt_short`] (first 5 bytes, lower hex).
        #[debug("{}", hex::encode(&from[..5]))]
        from: PeerIdBytes,
        /// Whether download policies require the content to be downloaded.
        should_download: bool,
        /// [`ContentStatus`] for this entry in the remote's replica.
        remote_content_status: ContentStatus,
    },
}

/// Whether an entry was inserted locally or by a remote peer.
#[derive(derive_more::Debug, Clone)]
pub enum InsertOrigin {
    /// The entry was inserted locally.
    Local,
    /// The entry was received from the remote node identified by [`PeerIdBytes`].
    Sync {
        /// The peer from which we received this entry.
        /// Debug matches [`iroh::PublicKey::fmt_short`] (first 5 bytes, lower hex).
        #[debug("{}", hex::encode(&from[..5]))]
        from: PeerIdBytes,
        /// Whether the peer claims to have the content blob for this entry.
        remote_content_status: ContentStatus,
    },
}

/// Whether the content status is available on a node.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContentStatus {
    /// The content is completely available.
    Complete,
    /// The content is partially available.
    Incomplete,
    /// The content is missing.
    Missing,
}

/// Outcome of a sync operation.
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// Timestamp of the latest entry for each author in the set we received.
    pub heads_received: AuthorHeads,
    /// Number of entries we received.
    pub num_recv: usize,
    /// Number of entries we sent.
    pub num_sent: usize,
}

fn get_as_ptr<T>(value: &T) -> Option<usize> {
    use std::mem;
    if mem::size_of::<T>() == std::mem::size_of::<usize>()
        && mem::align_of::<T>() == mem::align_of::<usize>()
    {
        // Safe only if size and alignment requirements are met
        unsafe { Some(mem::transmute_copy(value)) }
    } else {
        None
    }
}

fn same_channel<T>(a: &async_channel::Sender<T>, b: &async_channel::Sender<T>) -> bool {
    get_as_ptr(a).unwrap() == get_as_ptr(b).unwrap()
}

#[derive(Debug, Default)]
struct Subscribers(Vec<async_channel::Sender<Event>>);
impl Subscribers {
    pub fn subscribe(&mut self, sender: async_channel::Sender<Event>) {
        self.0.push(sender)
    }
    pub fn unsubscribe(&mut self, sender: &async_channel::Sender<Event>) {
        self.0.retain(|s| !same_channel(s, sender));
    }
    pub async fn send(&mut self, event: Event) {
        self.0 = std::mem::take(&mut self.0)
            .into_iter()
            .map(async |tx| tx.send(event.clone()).await.ok().map(|_| tx))
            .join_all()
            .await
            .into_iter()
            .flatten()
            .collect();
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub async fn send_with(&mut self, f: impl FnOnce() -> Event) {
        if !self.0.is_empty() {
            self.send(f()).await
        }
    }
}

/// Kind of capability of the namespace.
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    num_enum::IntoPrimitive,
    num_enum::TryFromPrimitive,
    strum::Display,
)]
#[repr(u8)]
#[strum(serialize_all = "snake_case")]
pub enum CapabilityKind {
    /// A writable replica.
    Write = 1,
    /// A readable replica.
    Read = 2,
}

/// The capability of the namespace.
#[derive(Debug, Clone, Serialize, Deserialize, derive_more::From)]
pub enum Capability {
    /// Write access to the namespace.
    Write(NamespaceSecret),
    /// Read only access to the namespace.
    Read(NamespaceId),
}

impl Capability {
    /// Get the [`NamespaceId`] for this [`Capability`].
    pub fn id(&self) -> NamespaceId {
        match self {
            Capability::Write(secret) => secret.id(),
            Capability::Read(id) => *id,
        }
    }

    /// Get the [`NamespaceSecret`] of this [`Capability`].
    /// Will fail if the [`Capability`] is read only.
    pub fn secret_key(&self) -> Result<&NamespaceSecret, ReadOnly> {
        match self {
            Capability::Write(secret) => Ok(secret),
            Capability::Read(_) => Err(ReadOnly),
        }
    }

    /// Get the kind of capability.
    pub fn kind(&self) -> CapabilityKind {
        match self {
            Capability::Write(_) => CapabilityKind::Write,
            Capability::Read(_) => CapabilityKind::Read,
        }
    }

    /// Get the raw representation of this namespace capability.
    pub fn raw(&self) -> (u8, [u8; 32]) {
        let capability_repr: u8 = self.kind().into();
        let bytes = match self {
            Capability::Write(secret) => secret.to_bytes(),
            Capability::Read(id) => id.to_bytes(),
        };
        (capability_repr, bytes)
    }

    /// Create a [`Capability`] from its raw representation.
    pub fn from_raw(kind: u8, bytes: &[u8; 32]) -> anyhow::Result<Self> {
        let kind: CapabilityKind = kind.try_into()?;
        let capability = match kind {
            CapabilityKind::Write => {
                let secret = NamespaceSecret::from_bytes(bytes);
                Capability::Write(secret)
            }
            CapabilityKind::Read => {
                let id = NamespaceId::from(bytes);
                Capability::Read(id)
            }
        };
        Ok(capability)
    }

    /// Merge this capability with another capability.
    ///
    /// Will return an error if `other` is not a capability for the same namespace.
    ///
    /// Returns `true` if the capability was changed, `false` otherwise.
    pub fn merge(&mut self, other: Capability) -> Result<bool, CapabilityError> {
        if other.id() != self.id() {
            return Err(CapabilityError::NamespaceMismatch);
        }

        // the only capability upgrade is from read-only (self) to writable (other)
        if matches!(self, Capability::Read(_)) && matches!(other, Capability::Write(_)) {
            let _ = std::mem::replace(self, other);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Errors for capability operations
#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    /// Namespaces are not the same
    #[error("Namespaces are not the same")]
    NamespaceMismatch,
}

/// In memory information about an open replica.
#[derive(derive_more::Debug)]
pub struct ReplicaInfo {
    pub(crate) capability: Capability,
    subscribers: Subscribers,
    #[debug("ContentStatusCallback")]
    content_status_cb: Option<ContentStatusCallback>,
    closed: bool,
}

impl ReplicaInfo {
    /// Create a new replica.
    pub fn new(capability: Capability) -> Self {
        Self {
            capability,
            subscribers: Default::default(),
            // on_insert_sender: RwLock::new(None),
            content_status_cb: None,
            closed: false,
        }
    }

    /// Subscribe to insert events.
    ///
    /// When subscribing to a replica, you must ensure that the corresponding [`async_channel::Receiver`] is
    /// received from in a loop. If not receiving, local and remote inserts will hang waiting for
    /// the receiver to be received from.
    pub fn subscribe(&mut self, sender: async_channel::Sender<Event>) {
        self.subscribers.subscribe(sender)
    }

    /// Explicitly unsubscribe a sender.
    ///
    /// Simply dropping the receiver is fine too. If you cloned a single sender to subscribe to
    /// multiple replicas, you can use this method to explicitly unsubscribe the sender from
    /// this replica without having to drop the receiver.
    pub fn unsubscribe(&mut self, sender: &async_channel::Sender<Event>) {
        self.subscribers.unsubscribe(sender)
    }

    /// Get the number of current event subscribers.
    pub fn subscribers_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Set the content status callback.
    ///
    /// Only one callback can be active at a time. If a previous callback was registered, this
    /// will return `false`.
    pub fn set_content_status_callback(&mut self, cb: ContentStatusCallback) -> bool {
        if self.content_status_cb.is_some() {
            false
        } else {
            self.content_status_cb = Some(cb);
            true
        }
    }

    fn ensure_open(&self) -> Result<(), InsertError> {
        if self.closed() {
            Err(InsertError::Closed)
        } else {
            Ok(())
        }
    }

    /// Returns true if the replica is closed.
    ///
    /// If a replica is closed, no further operations can be performed. A replica cannot be closed
    /// manually, it must be closed via [`store::Store::close_replica`] or
    /// [`store::Store::remove_replica`]
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// Merge a capability.
    ///
    /// The capability must refer to the the same namespace, otherwise an error will be returned.
    ///
    /// This will upgrade the replica's capability when passing a `Capability::Write`.
    /// It is a no-op if `capability` is a Capability::Read`.
    pub fn merge_capability(&mut self, capability: Capability) -> Result<bool, CapabilityError> {
        self.capability.merge(capability)
    }
}

/// Local representation of a mutable, synchronizable key-value store.
#[derive(derive_more::Debug)]
pub struct Replica<'a, I = Box<ReplicaInfo>> {
    pub(crate) store: StoreInstance<'a>,
    pub(crate) info: I,
}

impl<'a, I> Replica<'a, I>
where
    I: Deref<Target = ReplicaInfo> + DerefMut,
{
    /// Create a new replica.
    pub fn new(store: StoreInstance<'a>, info: I) -> Self {
        Replica { info, store }
    }

    /// Insert a new record at the given key.
    ///
    /// The entry will by signed by the provided `author`.
    /// The `len` must be the byte length of the data identified by `hash`.
    ///
    /// Returns the number of entries removed as a consequence of this insertion,
    /// or an error either if the entry failed to validate or if a store operation failed.
    pub async fn insert(
        &mut self,
        key: impl AsRef<[u8]>,
        author: &Author,
        hash: Hash,
        len: u64,
    ) -> Result<usize, InsertError> {
        if len == 0 || hash == Hash::EMPTY {
            return Err(InsertError::EntryIsEmpty);
        }
        self.info.ensure_open()?;
        let id = RecordIdentifier::new(self.id(), author.id(), key);
        let record = Record::new_current(hash, len);
        let entry = Entry::new(id, record);
        let secret = self.secret_key()?;
        let signed_entry = entry.sign(secret, author);
        self.insert_entry(signed_entry, InsertOrigin::Local).await
    }

    /// Delete entries that match the given `author` and key `prefix`.
    ///
    /// This inserts an empty entry with the key set to `prefix`, effectively clearing all other
    /// entries whose key starts with or is equal to the given `prefix`.
    ///
    /// Returns the number of entries deleted.
    pub async fn delete_prefix(
        &mut self,
        prefix: impl AsRef<[u8]>,
        author: &Author,
    ) -> Result<usize, InsertError> {
        self.info.ensure_open()?;
        let id = RecordIdentifier::new(self.id(), author.id(), prefix);
        let entry = Entry::new_empty(id);
        let signed_entry = entry.sign(self.secret_key()?, author);
        self.insert_entry(signed_entry, InsertOrigin::Local).await
    }

    /// Insert an entry into this replica which was received from a remote peer.
    ///
    /// This will verify both the namespace and author signatures of the entry, emit an `on_insert`
    /// event, and insert the entry into the replica store.
    ///
    /// Returns the number of entries removed as a consequence of this insertion,
    /// or an error if the entry failed to validate or if a store operation failed.
    pub async fn insert_remote_entry(
        &mut self,
        entry: SignedEntry,
        received_from: PeerIdBytes,
        content_status: ContentStatus,
    ) -> Result<usize, InsertError> {
        self.info.ensure_open()?;
        entry.validate_empty()?;
        let origin = InsertOrigin::Sync {
            from: received_from,
            remote_content_status: content_status,
        };
        self.insert_entry(entry, origin).await
    }

    /// Insert a signed entry into the database.
    ///
    /// Returns the number of entries removed as a consequence of this insertion.
    async fn insert_entry(
        &mut self,
        entry: SignedEntry,
        origin: InsertOrigin,
    ) -> Result<usize, InsertError> {
        let namespace = self.id();

        let store = &self.store;
        validate_entry(system_time_now(), store, namespace, &entry, &origin)?;

        let outcome = self.store.put(entry.clone()).map_err(InsertError::Store)?;
        tracing::debug!(?origin, hash = %entry.content_hash(), ?outcome, "insert");

        let removed_count = match outcome {
            InsertOutcome::Inserted { removed } => removed,
            InsertOutcome::NotInserted => return Err(InsertError::NewerEntryExists),
        };

        let insert_event = match origin {
            InsertOrigin::Local => Event::LocalInsert { namespace, entry },
            InsertOrigin::Sync {
                from,
                remote_content_status,
            } => {
                let download_policy = self
                    .store
                    .get_download_policy(&self.id())
                    .unwrap_or_default();
                let should_download = download_policy.matches(entry.entry());
                Event::RemoteInsert {
                    namespace,
                    entry,
                    from,
                    should_download,
                    remote_content_status,
                }
            }
        };

        self.info.subscribers.send(insert_event).await;

        Ok(removed_count)
    }

    /// Hashes the given data and inserts it.
    ///
    /// This does not store the content, just the record of it.
    /// Returns the calculated hash.
    pub async fn hash_and_insert(
        &mut self,
        key: impl AsRef<[u8]>,
        author: &Author,
        data: impl AsRef<[u8]>,
    ) -> Result<Hash, InsertError> {
        self.info.ensure_open()?;
        let len = data.as_ref().len() as u64;
        let hash = Hash::new(data);
        self.insert(key, author, hash, len).await?;
        Ok(hash)
    }

    /// Get the identifier for an entry in this replica.
    pub fn record_id(&self, key: impl AsRef<[u8]>, author: &Author) -> RecordIdentifier {
        RecordIdentifier::new(self.info.capability.id(), author.id(), key)
    }

    /// Create the initial message for the set reconciliation flow with a remote peer.
    pub fn sync_initial_message(&mut self) -> anyhow::Result<crate::ranger::Message<SignedEntry>> {
        self.info.ensure_open().map_err(anyhow::Error::from)?;
        self.store.initial_message()
    }

    /// Process a set reconciliation message from a remote peer.
    ///
    /// Returns the next message to be sent to the peer, if any.
    pub async fn sync_process_message(
        &mut self,
        message: crate::ranger::Message<SignedEntry>,
        from_peer: PeerIdBytes,
        state: &mut SyncOutcome,
    ) -> Result<Option<crate::ranger::Message<SignedEntry>>, anyhow::Error> {
        self.info.ensure_open()?;
        let my_namespace = self.id();
        let now = system_time_now();

        // update state with incoming data.
        state.num_recv += message.value_count();
        for (entry, _content_status) in message.values() {
            state
                .heads_received
                .insert(entry.author(), entry.timestamp());
        }

        // let subscribers = std::rc::Rc::new(&mut self.subscribers);
        // l
        let cb = self.info.content_status_cb.clone();
        let download_policy = self
            .store
            .get_download_policy(&my_namespace)
            .unwrap_or_default();
        let reply = self
            .store
            .process_message(
                &Default::default(),
                message,
                // validate callback: validate incoming entries, and send to on_insert channel
                |store, entry, content_status| {
                    let origin = InsertOrigin::Sync {
                        from: from_peer,
                        remote_content_status: content_status,
                    };
                    validate_entry(now, store, my_namespace, entry, &origin).is_ok()
                },
                // on_insert callback: is called when an entry was actually inserted in the store
                async |_store, entry, content_status| {
                    // We use `send_with` to only clone the entry if we have active subscriptions.
                    self.info
                        .subscribers
                        .send_with(|| {
                            let should_download = download_policy.matches(entry.entry());
                            Event::RemoteInsert {
                                from: from_peer,
                                namespace: my_namespace,
                                entry: entry.clone(),
                                should_download,
                                remote_content_status: content_status,
                            }
                        })
                        .await
                },
                // content_status callback: get content status for outgoing entries
                async move |entry| {
                    if let Some(cb) = cb.as_ref() {
                        cb(entry.content_hash()).await
                    } else {
                        ContentStatus::Missing
                    }
                },
            )
            .await?;

        // update state with outgoing data.
        if let Some(ref reply) = reply {
            state.num_sent += reply.value_count();
        }

        Ok(reply)
    }

    /// Get the namespace identifier for this [`Replica`].
    pub fn id(&self) -> NamespaceId {
        self.info.capability.id()
    }

    /// Get the [`Capability`] of this [`Replica`].
    pub fn capability(&self) -> &Capability {
        &self.info.capability
    }

    /// Get the byte representation of the [`NamespaceSecret`] key for this replica. Will fail if
    /// the replica is read only
    pub fn secret_key(&self) -> Result<&NamespaceSecret, ReadOnly> {
        self.info.capability.secret_key()
    }
}

/// Error that occurs trying to access the [`NamespaceSecret`] of a read-only [`Capability`].
#[derive(Debug, thiserror::Error)]
#[error("Replica allows read access only.")]
pub struct ReadOnly;

/// Validate a [`SignedEntry`] if it's fit to be inserted.
///
/// This validates that
/// * the entry's author and namespace signatures are correct
/// * the entry's namespace matches the current replica
/// * the entry's timestamp is not more than 10 minutes in the future of our system time
/// * the entry is newer than an existing entry for the same key and author, if such exists.
fn validate_entry<S: ranger::Store<SignedEntry> + PublicKeyStore>(
    now: u64,
    store: &S,
    expected_namespace: NamespaceId,
    entry: &SignedEntry,
    origin: &InsertOrigin,
) -> Result<(), ValidationFailure> {
    // Verify the namespace
    if entry.namespace() != expected_namespace {
        return Err(ValidationFailure::InvalidNamespace);
    }

    // Verify signature for non-local entries.
    if !matches!(origin, InsertOrigin::Local) && entry.verify(store).is_err() {
        return Err(ValidationFailure::BadSignature);
    }

    // Verify that the timestamp of the entry is not too far in the future.
    if entry.timestamp() > now + MAX_TIMESTAMP_FUTURE_SHIFT {
        return Err(ValidationFailure::TooFarInTheFuture);
    }
    Ok(())
}

/// Error emitted when inserting entries into a [`Replica`] failed
#[derive(thiserror::Error, derive_more::Debug, derive_more::From)]
pub enum InsertError {
    /// Storage error
    #[error("storage error")]
    Store(anyhow::Error),
    /// Validation failure
    #[error("validation failure")]
    Validation(#[from] ValidationFailure),
    /// A newer entry exists for either this entry's key or a prefix of the key.
    #[error("A newer entry exists for either this entry's key or a prefix of the key.")]
    NewerEntryExists,
    /// Attempted to insert an empty entry.
    #[error("Attempted to insert an empty entry")]
    EntryIsEmpty,
    /// Replica is read only.
    #[error("Attempted to insert to read only replica")]
    #[from(ReadOnly)]
    ReadOnly,
    /// The replica is closed, no operations may be performed.
    #[error("replica is closed")]
    Closed,
}

/// Reason why verifying a [`SignedEntry`] failed.
#[derive(thiserror::Error, Debug)]
pub enum SignedEntryVerifyError {
    /// One of the entry's public key bytes is not a valid ed25519 curve point.
    #[error(transparent)]
    KeyParsing(#[from] KeyParsingError),
    /// One of the entry's signatures failed verification against the recovered key.
    #[error(transparent)]
    Signature(#[from] SignatureError),
}

/// Reason why entry validation failed
#[derive(thiserror::Error, Debug)]
pub enum ValidationFailure {
    /// Entry namespace does not match the current replica.
    #[error("Entry namespace does not match the current replica")]
    InvalidNamespace,
    /// Entry signature is invalid.
    #[error("Entry signature is invalid")]
    BadSignature,
    /// Entry timestamp is too far in the future.
    #[error("Entry timestamp is too far in the future.")]
    TooFarInTheFuture,
    /// Entry has length 0 but not the empty hash, or the empty hash but not length 0.
    #[error("Entry has length 0 but not the empty hash, or the empty hash but not length 0")]
    InvalidEmptyEntry,
}

/// A signed entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedEntry {
    signature: EntrySignature,
    entry: Entry,
}

impl From<SignedEntry> for Entry {
    fn from(value: SignedEntry) -> Self {
        value.entry
    }
}

impl PartialOrd for SignedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SignedEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.entry.cmp(&other.entry)
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id
            .cmp(&other.id)
            .then_with(|| self.record.cmp(&other.record))
    }
}

impl SignedEntry {
    pub(crate) fn new(signature: EntrySignature, entry: Entry) -> Self {
        SignedEntry { signature, entry }
    }

    /// Create a new signed entry by signing an entry with the `namespace` and `author`.
    pub fn from_entry(entry: Entry, namespace: &NamespaceSecret, author: &Author) -> Self {
        let signature = EntrySignature::from_entry(&entry, namespace, author);
        SignedEntry { signature, entry }
    }

    /// Create a new signed entries from its parts.
    pub fn from_parts(
        namespace: &NamespaceSecret,
        author: &Author,
        key: impl AsRef<[u8]>,
        record: Record,
    ) -> Self {
        let id = RecordIdentifier::new(namespace.id(), author.id(), key);
        let entry = Entry::new(id, record);
        Self::from_entry(entry, namespace, author)
    }

    /// Verify the signatures on this entry.
    pub fn verify<S: store::PublicKeyStore>(
        &self,
        store: &S,
    ) -> Result<(), SignedEntryVerifyError> {
        self.signature.verify(
            &self.entry,
            &self.entry.namespace().public_key(store)?,
            &self.entry.author().public_key(store)?,
        )?;
        Ok(())
    }

    /// Get the signature.
    pub fn signature(&self) -> &EntrySignature {
        &self.signature
    }

    /// Validate that the entry has the empty hash if the length is 0, or a non-zero length.
    pub fn validate_empty(&self) -> Result<(), ValidationFailure> {
        self.entry().validate_empty()
    }

    /// Get the [`Entry`].
    pub fn entry(&self) -> &Entry {
        &self.entry
    }

    /// Get the content [`struct@Hash`] of the entry.
    pub fn content_hash(&self) -> Hash {
        self.entry().content_hash()
    }

    /// Get the content length of the entry.
    pub fn content_len(&self) -> u64 {
        self.entry().content_len()
    }

    /// Get the author bytes of this entry.
    pub fn author_bytes(&self) -> AuthorId {
        self.entry().id().author()
    }

    /// Get the key of the entry.
    pub fn key(&self) -> &[u8] {
        self.entry().id().key()
    }

    /// Get the timestamp of the entry.
    pub fn timestamp(&self) -> u64 {
        self.entry().timestamp()
    }
}

impl RangeEntry for SignedEntry {
    type Key = RecordIdentifier;
    type Value = Record;

    fn key(&self) -> &Self::Key {
        &self.entry.id
    }

    fn value(&self) -> &Self::Value {
        &self.entry.record
    }

    fn as_fingerprint(&self) -> crate::ranger::Fingerprint {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.namespace().as_ref());
        hasher.update(self.author_bytes().as_ref());
        hasher.update(self.key());
        hasher.update(&self.timestamp().to_be_bytes());
        hasher.update(self.content_hash().as_bytes());
        Fingerprint(hasher.finalize().into())
    }
}

/// Signature over an entry.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntrySignature {
    author_signature: Signature,
    namespace_signature: Signature,
}

impl Debug for EntrySignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntrySignature")
            .field(
                "namespace_signature",
                &hex::encode(self.namespace_signature.to_bytes()),
            )
            .field(
                "author_signature",
                &hex::encode(self.author_signature.to_bytes()),
            )
            .finish()
    }
}

impl EntrySignature {
    /// Create a new signature by signing an entry with the `namespace` and `author`.
    pub fn from_entry(entry: &Entry, namespace: &NamespaceSecret, author: &Author) -> Self {
        // TODO: this should probably include a namespace prefix
        // namespace in the cryptographic sense.
        let bytes = entry.to_vec();
        let namespace_signature = Signature::from_bytes(&namespace.sign(&bytes).to_bytes());
        let author_signature = Signature::from_bytes(&author.sign(&bytes).to_bytes());

        EntrySignature {
            author_signature,
            namespace_signature,
        }
    }

    /// Verify that this signature was created by signing the `entry` with the
    /// secret keys of the specified `author` and `namespace`.
    pub fn verify(
        &self,
        entry: &Entry,
        namespace: &NamespacePublicKey,
        author: &AuthorPublicKey,
    ) -> Result<(), SignatureError> {
        let bytes = entry.to_vec();
        namespace.verify(&bytes, &self.namespace_signature)?;
        author.verify(&bytes, &self.author_signature)?;

        Ok(())
    }

    pub(crate) fn from_parts(namespace_sig: &[u8; 64], author_sig: &[u8; 64]) -> Self {
        let namespace_signature = Signature::from_bytes(namespace_sig);
        let author_signature = Signature::from_bytes(author_sig);

        EntrySignature {
            author_signature,
            namespace_signature,
        }
    }

    pub(crate) fn author(&self) -> &Signature {
        &self.author_signature
    }

    pub(crate) fn namespace(&self) -> &Signature {
        &self.namespace_signature
    }
}

/// A single entry in a [`Replica`]
///
/// An entry is identified by a key, its [`Author`], and the [`Replica`]'s
/// [`NamespaceSecret`]. Its value is the [32-byte BLAKE3 hash](iroh_blobs::Hash)
/// of the entry's content data, the size of this content data, and a timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    id: RecordIdentifier,
    record: Record,
}

impl Entry {
    /// Create a new entry
    pub fn new(id: RecordIdentifier, record: Record) -> Self {
        Entry { id, record }
    }

    /// Create a new empty entry with the current timestamp.
    pub fn new_empty(id: RecordIdentifier) -> Self {
        Entry {
            id,
            record: Record::empty_current(),
        }
    }

    /// Validate that the entry has the empty hash if the length is 0, or a non-zero length.
    pub fn validate_empty(&self) -> Result<(), ValidationFailure> {
        match (self.content_hash() == Hash::EMPTY, self.content_len() == 0) {
            (true, true) => Ok(()),
            (false, false) => Ok(()),
            (true, false) => Err(ValidationFailure::InvalidEmptyEntry),
            (false, true) => Err(ValidationFailure::InvalidEmptyEntry),
        }
    }

    /// Get the [`RecordIdentifier`] for this entry.
    pub fn id(&self) -> &RecordIdentifier {
        &self.id
    }

    /// Get the [`NamespaceId`] of this entry.
    pub fn namespace(&self) -> NamespaceId {
        self.id.namespace()
    }

    /// Get the [`AuthorId`] of this entry.
    pub fn author(&self) -> AuthorId {
        self.id.author()
    }

    /// Get the key of this entry.
    pub fn key(&self) -> &[u8] {
        self.id.key()
    }

    /// Get the [`Record`] contained in this entry.
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// Get the content hash of the record.
    pub fn content_hash(&self) -> Hash {
        self.record.hash
    }

    /// Get the content length of the record.
    pub fn content_len(&self) -> u64 {
        self.record.len
    }

    /// Get the timestamp of the record.
    pub fn timestamp(&self) -> u64 {
        self.record.timestamp
    }

    /// Serialize this entry into its canonical byte representation used for signing.
    pub fn encode(&self, out: &mut Vec<u8>) {
        self.id.encode(out);
        self.record.encode(out);
    }

    /// Serialize this entry into a new vector with its canonical byte representation.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    /// Sign this entry with a [`NamespaceSecret`] and [`Author`].
    pub fn sign(self, namespace: &NamespaceSecret, author: &Author) -> SignedEntry {
        SignedEntry::from_entry(self, namespace, author)
    }
}

const NAMESPACE_BYTES: std::ops::Range<usize> = 0..32;
const AUTHOR_BYTES: std::ops::Range<usize> = 32..64;
const KEY_BYTES: std::ops::RangeFrom<usize> = 64..;

/// The identifier of a record.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordIdentifier(Bytes);

impl Default for RecordIdentifier {
    fn default() -> Self {
        Self::new(NamespaceId::default(), AuthorId::default(), b"")
    }
}

impl Debug for RecordIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordIdentifier")
            .field("namespace", &self.namespace())
            .field("author", &self.author())
            .field("key", &std::string::String::from_utf8_lossy(self.key()))
            .finish()
    }
}

impl RangeKey for RecordIdentifier {
    #[cfg(test)]
    fn is_prefix_of(&self, other: &Self) -> bool {
        other.as_ref().starts_with(self.as_ref())
    }
}

fn system_time_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("time drift")
        .as_micros() as u64
}

impl RecordIdentifier {
    /// Create a new [`RecordIdentifier`].
    pub fn new(
        namespace: impl Into<NamespaceId>,
        author: impl Into<AuthorId>,
        key: impl AsRef<[u8]>,
    ) -> Self {
        let mut bytes = BytesMut::with_capacity(32 + 32 + key.as_ref().len());
        bytes.extend_from_slice(namespace.into().as_bytes());
        bytes.extend_from_slice(author.into().as_bytes());
        bytes.extend_from_slice(key.as_ref());
        Self(bytes.freeze())
    }

    /// Serialize this [`RecordIdentifier`] into a mutable byte array.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }

    /// Get this [`RecordIdentifier`] as [Bytes].
    pub fn as_bytes(&self) -> Bytes {
        self.0.clone()
    }

    /// Get this [`RecordIdentifier`] as a tuple of byte slices.
    pub fn as_byte_tuple(&self) -> (&[u8; 32], &[u8; 32], &[u8]) {
        (
            self.0[NAMESPACE_BYTES].try_into().unwrap(),
            self.0[AUTHOR_BYTES].try_into().unwrap(),
            &self.0[KEY_BYTES],
        )
    }

    /// Get this [`RecordIdentifier`] as a tuple of bytes.
    pub fn to_byte_tuple(&self) -> ([u8; 32], [u8; 32], Bytes) {
        (
            self.0[NAMESPACE_BYTES].try_into().unwrap(),
            self.0[AUTHOR_BYTES].try_into().unwrap(),
            self.0.slice(KEY_BYTES),
        )
    }

    /// Get the key of this record.
    pub fn key(&self) -> &[u8] {
        &self.0[KEY_BYTES]
    }

    /// Get the key of this record as [`Bytes`].
    pub fn key_bytes(&self) -> Bytes {
        self.0.slice(KEY_BYTES)
    }

    /// Get the [`NamespaceId`] of this record as byte array.
    pub fn namespace(&self) -> NamespaceId {
        let value: &[u8; 32] = &self.0[NAMESPACE_BYTES].try_into().unwrap();
        value.into()
    }

    /// Get the [`AuthorId`] of this record as byte array.
    pub fn author(&self) -> AuthorId {
        let value: &[u8; 32] = &self.0[AUTHOR_BYTES].try_into().unwrap();
        value.into()
    }
}

impl AsRef<[u8]> for RecordIdentifier {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Deref for SignedEntry {
    type Target = Entry;
    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl Deref for Entry {
    type Target = Record;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}

/// The data part of an entry in a [`Replica`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    /// Length of the data referenced by `hash`.
    len: u64,
    /// Hash of the content data.
    hash: Hash,
    /// Record creation timestamp. Counted as micros since the Unix epoch.
    timestamp: u64,
}

impl RangeValue for Record {}

/// Ordering for entry values.
///
/// Compares first the timestamp, then the content hash.
impl Ord for Record {
    fn cmp(&self, other: &Self) -> Ordering {
        self.timestamp
            .cmp(&other.timestamp)
            .then_with(|| self.hash.cmp(&other.hash))
    }
}

impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Record {
    /// Create a new record.
    pub fn new(hash: Hash, len: u64, timestamp: u64) -> Self {
        debug_assert!(
            len != 0 || hash == Hash::EMPTY,
            "if `len` is 0 then `hash` must be the hash of the empty byte range"
        );
        Record {
            hash,
            len,
            timestamp,
        }
    }

    /// Create a tombstone record (empty content)
    pub fn empty(timestamp: u64) -> Self {
        Self::new(Hash::EMPTY, 0, timestamp)
    }

    /// Create a tombstone record with the timestamp set to now.
    pub fn empty_current() -> Self {
        Self::new_current(Hash::EMPTY, 0)
    }

    /// Return `true` if the entry is empty.
    pub fn is_empty(&self) -> bool {
        self.hash == Hash::EMPTY
    }

    /// Create a new [`Record`] with the timestamp set to now.
    pub fn new_current(hash: Hash, len: u64) -> Self {
        let timestamp = system_time_now();
        Self::new(hash, len, timestamp)
    }

    /// Get the length of the data addressed by this record's content hash.
    pub fn content_len(&self) -> u64 {
        self.len
    }

    /// Get the [`struct@Hash`] of the content data of this record.
    pub fn content_hash(&self) -> Hash {
        self.hash
    }

    /// Get the timestamp of this record.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    #[cfg(test)]
    pub(crate) fn current_from_data(data: impl AsRef<[u8]>) -> Self {
        let len = data.as_ref().len() as u64;
        let hash = Hash::new(data);
        Self::new_current(hash, len)
    }

    #[cfg(test)]
    pub(crate) fn from_data(data: impl AsRef<[u8]>, timestamp: u64) -> Self {
        let len = data.as_ref().len() as u64;
        let hash = Hash::new(data);
        Self::new(hash, len, timestamp)
    }

    /// Serialize this record into a mutable byte array.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.len.to_be_bytes());
        out.extend_from_slice(self.hash.as_ref());
        out.extend_from_slice(&self.timestamp.to_be_bytes())
    }
}


