//! Implementation of Set Reconcilliation based on
//! "Range-Based Set Reconciliation" by Aljoscha Meyer.

use std::fmt::Debug;

use n0_future::StreamExt;
use serde::{Deserialize, Serialize};

use crate::ContentStatus;

/// Store entries that can be fingerprinted and put into ranges.
pub trait RangeEntry: Debug + Clone {
    /// The key type for this entry.
    ///
    /// This type must implement [`Ord`] to define the range ordering used in the set
    /// reconciliation algorithm.
    ///
    /// See [`RangeKey`] for details.
    type Key: RangeKey;

    /// The value type for this entry. See
    ///
    /// The type must implement [`Ord`] to define the time ordering of entries used in the prefix
    /// deletion algorithm.
    ///
    /// See [`RangeValue`] for details.
    type Value: RangeValue;

    /// Get the key for this entry.
    fn key(&self) -> &Self::Key;

    /// Get the value for this entry.
    fn value(&self) -> &Self::Value;

    /// Get the fingerprint for this entry.
    fn as_fingerprint(&self) -> Fingerprint;
}

/// A trait constraining types that are valid entry keys.
pub trait RangeKey: Sized + Debug + Ord + PartialEq + Clone + 'static {
    /// Returns `true` if `self` is a prefix of `other`.
    #[cfg(test)]
    fn is_prefix_of(&self, other: &Self) -> bool;

    /// Returns true if `other` is a prefix of `self`.
    #[cfg(test)]
    fn is_prefixed_by(&self, other: &Self) -> bool {
        other.is_prefix_of(self)
    }
}

/// A trait constraining types that are valid entry values.
pub trait RangeValue: Sized + Debug + Ord + PartialEq + Clone + 'static {}

/// Stores a range.
///
/// There are three possibilities
///
/// - x, x: All elements in a set, denoted with
/// - [x, y): x < y: Includes x, but not y
/// - S \ [y, x) y < x: Includes x, but not y.
///
/// This means that ranges are "wrap around" conceptually.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub(crate) struct Range<K> {
    x: K,
    y: K,
}

impl<K> Range<K> {
    pub(crate) fn x(&self) -> &K {
        &self.x
    }

    pub(crate) fn y(&self) -> &K {
        &self.y
    }

    pub(crate) fn new(x: K, y: K) -> Self {
        Range { x, y }
    }
}

impl<K: Ord> Range<K> {
    pub(crate) fn is_all(&self) -> bool {
        self.x() == self.y()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, t: &K) -> bool {
        use std::cmp::Ordering;
        match self.x().cmp(self.y()) {
            Ordering::Equal => true,
            Ordering::Less => self.x() <= t && t < self.y(),
            Ordering::Greater => self.x() <= t || t < self.y(),
        }
    }
}

impl<K> From<(K, K)> for Range<K> {
    fn from((x, y): (K, K)) -> Self {
        Range { x, y }
    }
}

#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint(pub [u8; 32]);

impl Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fp({})", blake3::Hash::from(self.0).to_hex())
    }
}

impl Fingerprint {
    /// The fingerprint of the empty set
    pub(crate) fn empty() -> Self {
        Fingerprint(*blake3::hash(&[]).as_bytes())
    }
}

impl std::ops::BitXorAssign for Fingerprint {
    fn bitxor_assign(&mut self, rhs: Self) {
        for (a, b) in self.0.iter_mut().zip(rhs.0.iter()) {
            *a ^= b;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeFingerprint<K> {
    #[serde(bound(
        serialize = "Range<K>: Serialize",
        deserialize = "Range<K>: Deserialize<'de>"
    ))]
    pub(crate) range: Range<K>,
    /// The fingerprint of `range`.
    pub(crate) fingerprint: Fingerprint,
}

/// Transfers items inside a range to the other participant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeItem<E: RangeEntry> {
    /// The range out of which the elements are.
    #[serde(bound(
        serialize = "Range<E::Key>: Serialize",
        deserialize = "Range<E::Key>: Deserialize<'de>"
    ))]
    pub(crate) range: Range<E::Key>,
    #[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
    pub(crate) values: Vec<(E, ContentStatus)>,
    /// If false, requests to send local items in the range.
    /// Otherwise not.
    pub(crate) have_local: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessagePart<E: RangeEntry> {
    #[serde(bound(
        serialize = "RangeFingerprint<E::Key>: Serialize",
        deserialize = "RangeFingerprint<E::Key>: Deserialize<'de>"
    ))]
    RangeFingerprint(RangeFingerprint<E::Key>),
    #[serde(bound(
        serialize = "RangeItem<E>: Serialize",
        deserialize = "RangeItem<E>: Deserialize<'de>"
    ))]
    RangeItem(RangeItem<E>),
}

impl<E: RangeEntry> MessagePart<E> {
    #[cfg(test)]
    pub(crate) fn is_range_fingerprint(&self) -> bool {
        matches!(self, MessagePart::RangeFingerprint(_))
    }

    #[cfg(test)]
    pub(crate) fn is_range_item(&self) -> bool {
        matches!(self, MessagePart::RangeItem(_))
    }

    pub(crate) fn values(&self) -> Option<&[(E, ContentStatus)]> {
        match self {
            MessagePart::RangeFingerprint(_) => None,
            MessagePart::RangeItem(RangeItem { values, .. }) => Some(values),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message<E: RangeEntry> {
    #[serde(bound(
        serialize = "MessagePart<E>: Serialize",
        deserialize = "MessagePart<E>: Deserialize<'de>"
    ))]
    parts: Vec<MessagePart<E>>,
}

impl<E: RangeEntry> Message<E> {
    /// Construct the initial message.
    fn init<S: Store<E>>(store: &mut S) -> Result<Self, S::Error> {
        let x = store.get_first()?;
        let range = Range::new(x.clone(), x);
        let fingerprint = store.get_fingerprint(&range)?;
        let part = MessagePart::RangeFingerprint(RangeFingerprint { range, fingerprint });
        Ok(Message { parts: vec![part] })
    }

    pub(crate) fn parts(&self) -> &[MessagePart<E>] {
        &self.parts
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &(E, ContentStatus)> {
        self.parts().iter().filter_map(|p| p.values()).flatten()
    }

    pub(crate) fn value_count(&self) -> usize {
        self.values().count()
    }
}

pub trait Store<E: RangeEntry>: Sized {
    type Error: Debug + Send + Sync + Into<anyhow::Error> + 'static;

    type RangeIterator<'a>: Iterator<Item = Result<E, Self::Error>>
    where
        Self: 'a,
        E: 'a;

    type ParentIterator<'a>: Iterator<Item = Result<E, Self::Error>>
    where
        Self: 'a,
        E: 'a;

    /// Get a the first key (or the default if none is available).
    fn get_first(&mut self) -> Result<E::Key, Self::Error>;

    /// Get a single entry.
    #[cfg(test)]
    fn get(&mut self, key: &E::Key) -> Result<Option<E>, Self::Error>;

    /// Get the number of entries in the store.
    #[cfg(test)]
    fn len(&mut self) -> Result<usize, Self::Error>;

    /// Returns `true` if the vector contains no elements.
    #[cfg(test)]
    #[allow(unused)]
    fn is_empty(&mut self) -> Result<bool, Self::Error>;

    /// Calculate the fingerprint of the given range.
    fn get_fingerprint(&mut self, range: &Range<E::Key>) -> Result<Fingerprint, Self::Error>;

    /// Insert just the given key value pair.
    ///
    /// This will replace just the existing entry, but will not perform prefix
    /// deletion.
    fn entry_put(&mut self, entry: E) -> Result<(), Self::Error>;

    /// Returns all entries in the given range.
    fn get_range(&mut self, range: Range<E::Key>) -> Result<Self::RangeIterator<'_>, Self::Error>;

    /// Returns the number of entries in the range.
    ///
    /// Default impl is not optimized, but does avoid excessive memory usage.
    fn get_range_len(&mut self, range: Range<E::Key>) -> Result<usize, Self::Error> {
        let mut count = 0;
        for el in self.get_range(range)? {
            let _el = el?;
            count += 1;
        }
        Ok(count)
    }

    /// Returns all entries whose key starts with the given `prefix`.
    #[cfg(test)]
    #[allow(unused)]
    fn prefixed_by(&mut self, prefix: &E::Key) -> Result<Self::RangeIterator<'_>, Self::Error>;

    /// Returns all entries that share a prefix with `key`, including the entry for `key` itself.
    fn prefixes_of(&mut self, key: &E::Key) -> Result<Self::ParentIterator<'_>, Self::Error>;

    /// Get all entries in the store
    #[cfg(test)]
    fn all(&mut self) -> Result<Self::RangeIterator<'_>, Self::Error>;

    /// Remove an entry from the store.
    ///
    /// This will remove just the entry with the given key, but will not perform prefix deletion.
    #[cfg(test)]
    #[allow(unused)]
    fn entry_remove(&mut self, key: &E::Key) -> Result<Option<E>, Self::Error>;

    /// Remove all entries whose key start with a prefix and for which the `predicate` callback
    /// returns true.
    ///
    /// Returns the number of elements removed.
    // TODO: We might want to return an iterator with the removed elements instead to emit as
    // events to the application potentially.
    fn remove_prefix_filtered(
        &mut self,
        prefix: &E::Key,
        predicate: impl Fn(&E::Value) -> bool,
    ) -> Result<usize, Self::Error>;

    /// Generates the initial message.
    fn initial_message(&mut self) -> Result<Message<E>, Self::Error> {
        Message::init(self)
    }

    /// Processes an incoming message and produces a response.
    /// If terminated, returns `None`
    ///
    /// `validate_cb` is called for each incoming entry received from the remote.
    /// It must return true if the entry is valid and should be stored, and false otherwise
    /// (which means the entry will be dropped and not stored).
    ///
    /// `on_insert_cb` is called for each entry that was actually inserted into the store (so not
    /// for entries which validated, but are not inserted because they are older than one of their
    /// prefixes).
    ///
    /// `content_status_cb` is called for each outgoing entry about to be sent to the remote.
    /// It must return a [`ContentStatus`], which will be sent to the remote with the entry.
    async fn process_message<F, F2, F3>(
        &mut self,
        config: &SyncConfig,
        message: Message<E>,
        validate_cb: F,
        mut on_insert_cb: F2,
        content_status_cb: F3,
    ) -> Result<Option<Message<E>>, Self::Error>
    where
        F: Fn(&Self, &E, ContentStatus) -> bool,
        F2: AsyncFnMut(&Self, E, ContentStatus),
        F3: for<'a> AsyncFn(&'a E) -> ContentStatus,
    {
        let mut out = Vec::new();

        // TODO: can these allocs be avoided?
        let mut items = Vec::new();
        let mut fingerprints = Vec::new();
        for part in message.parts {
            match part {
                MessagePart::RangeItem(item) => {
                    items.push(item);
                }
                MessagePart::RangeFingerprint(fp) => {
                    fingerprints.push(fp);
                }
            }
        }

        // Process item messages
        for RangeItem {
            range,
            values,
            have_local,
        } in items
        {
            let diff: Option<Vec<_>> = if have_local {
                None
            } else {
                Some({
                    // we get the range of the item form our store. from this set, we remove all
                    // entries that whose key is contained in the peer's set and where our value is
                    // lower than the peer entry's value.
                    let items = self
                        .get_range(range.clone())?
                        .filter_map(|our_entry| match our_entry {
                            Ok(our_entry) => {
                                if !values.iter().any(|(their_entry, _)| {
                                    our_entry.key() == their_entry.key()
                                        && their_entry.value() >= our_entry.value()
                                }) {
                                    Some(Ok(our_entry))
                                } else {
                                    None
                                }
                            }
                            Err(err) => Some(Err(err)),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    // add the content status in a second pass
                    let values = items.into_iter().map(|entry| async {
                        let content_status = content_status_cb(&entry).await;
                        (entry, content_status)
                    });
                    n0_future::FuturesOrdered::from_iter(values).collect().await
                })
            };

            // Store incoming values
            for (entry, content_status) in values {
                if validate_cb(self, &entry, content_status) {
                    // TODO: Get rid of the clone?
                    let outcome = self.put(entry.clone())?;
                    if let InsertOutcome::Inserted { .. } = outcome {
                        on_insert_cb(self, entry, content_status).await;
                    }
                }
            }

            if let Some(diff) = diff {
                if !diff.is_empty() {
                    out.push(MessagePart::RangeItem(RangeItem {
                        range,
                        values: diff,
                        have_local: true,
                    }));
                }
            }
        }

        // Process fingerprint messages
        for RangeFingerprint { range, fingerprint } in fingerprints {
            let local_fingerprint = self.get_fingerprint(&range)?;
            // Case1 Match, nothing to do
            if local_fingerprint == fingerprint {
                continue;
            }

            // Case2 Recursion Anchor
            let num_local_values = self.get_range_len(range.clone())?;
            if num_local_values <= 1 || fingerprint == Fingerprint::empty() {
                let values = self
                    .get_range(range.clone())?
                    .collect::<Result<Vec<_>, _>>()?;
                let values = values.into_iter().map(|entry| async {
                    let content_status = content_status_cb(&entry).await;
                    (entry, content_status)
                });
                let values = n0_future::FuturesOrdered::from_iter(values).collect().await;
                out.push(MessagePart::RangeItem(RangeItem {
                    range,
                    values,
                    have_local: false,
                }));
            } else {
                // Case3 Recurse
                // Create partition
                // m0 = x < m1 < .. < mk = y, with k>= 2
                // such that [ml, ml+1) is nonempty
                let mut ranges = Vec::with_capacity(config.split_factor);

                // Select the first index, for which the key is larger or equal than the x of the range.
                let mut start_index = 0;
                for el in self.get_range(range.clone())? {
                    let el = el?;
                    if el.key() >= range.x() {
                        break;
                    }
                    start_index += 1;
                }

                // select a pivot value. pivots repeat every split_factor, so pivot(i) == pivot(i + self.split_factor * x)
                // it is guaranteed that pivot(0) != x if local_values.len() >= 2
                let mut pivot = |i: usize| {
                    // ensure that pivots wrap around
                    let i = i % config.split_factor;
                    // choose an offset. this will be
                    // 1/2, 1 in case of split_factor == 2
                    // 1/3, 2/3, 1 in case of split_factor == 3
                    // etc.
                    let offset = (num_local_values * (i + 1)) / config.split_factor;
                    let offset = (start_index + offset) % num_local_values;
                    self.get_range(range.clone())
                        .map(|mut i| i.nth(offset))
                        .and_then(|e| e.expect("missing entry"))
                        .map(|e| e.key().clone())
                };
                if range.is_all() {
                    // the range is the whole set, so range.x and range.y should not matter
                    // just add all ranges as normal ranges. Exactly one of the ranges will
                    // wrap around, so we cover the entire set.
                    for i in 0..config.split_factor {
                        let (x, y) = (pivot(i)?, pivot(i + 1)?);
                        // don't push empty ranges
                        if x != y {
                            ranges.push(Range { x, y })
                        }
                    }
                } else {
                    // guaranteed to be non-empty because
                    // - pivot(0) is guaranteed to be != x for local_values.len() >= 2
                    // - local_values.len() < 2 gets handled by the recursion anchor
                    // - x != y (regular range)
                    ranges.push(Range {
                        x: range.x().clone(),
                        y: pivot(0)?,
                    });
                    // this will only be executed for split_factor > 2
                    for i in 0..config.split_factor - 2 {
                        // don't push empty ranges
                        let (x, y) = (pivot(i)?, pivot(i + 1)?);
                        if x != y {
                            ranges.push(Range { x, y })
                        }
                    }
                    // guaranteed to be non-empty because
                    // - pivot is a value in the range
                    // - y is the exclusive end of the range
                    // - x != y (regular range)
                    ranges.push(Range {
                        x: pivot(config.split_factor - 2)?,
                        y: range.y().clone(),
                    });
                }

                let mut non_empty = 0;
                for range in ranges {
                    let chunk: Vec<_> = self.get_range(range.clone())?.collect();
                    if !chunk.is_empty() {
                        non_empty += 1;
                    }
                    // Add either the fingerprint or the item set
                    let fingerprint = self.get_fingerprint(&range)?;
                    if chunk.len() > config.max_set_size {
                        out.push(MessagePart::RangeFingerprint(RangeFingerprint {
                            range: range.clone(),
                            fingerprint,
                        }));
                    } else {
                        // let content_status_cb = content_status_cb.clone();
                        let values = chunk.into_iter().filter_map(|entry| match entry {
                            Err(_err) => None,
                            Ok(entry) => Some(async {
                                let content_status = content_status_cb(&entry).await;
                                (entry, content_status)
                            }),
                        });
                        let values = n0_future::FuturesOrdered::from_iter(values).collect().await;
                        out.push(MessagePart::RangeItem(RangeItem {
                            range,
                            values,
                            have_local: false,
                        }));
                    }
                }
                debug_assert!(non_empty > 1);
            }
        }

        // If we have any parts, return a message
        if !out.is_empty() {
            Ok(Some(Message { parts: out }))
        } else {
            Ok(None)
        }
    }

    /// Insert a key value pair.
    ///
    /// Entries are inserted if they compare strictly greater than all entries in the set of
    /// entries which have the same key as `entry` or have a key which is a prefix of `entry`.
    ///
    /// Additionally, entries that have a key which is a prefix of the entry's key and whose
    /// timestamp is not strictly greater than that of the new entry are deleted
    ///
    /// Note: The deleted entries are simply dropped right now. We might want to make this return
    /// an iterator, to potentially log or expose the deleted entries.
    ///
    /// Returns `true` if the entry was inserted.
    /// Returns `false` if it was not inserted.
    fn put(&mut self, entry: E) -> Result<InsertOutcome, Self::Error> {
        let prefix_entry = self.prefixes_of(entry.key())?;
        // First we check if our entry is strictly greater than all parent elements.
        // From the willow spec:
        // "Remove all entries whose timestamp is strictly less than the timestamp of any other entry [..]
        // whose path is a prefix of p." and then "remove all but those whose record has the greatest hash component".
        // This is the contract of the `Ord` impl for `E::Value`.
        for prefix_entry in prefix_entry {
            let prefix_entry = prefix_entry?;
            if entry.value() <= prefix_entry.value() {
                return Ok(InsertOutcome::NotInserted);
            }
        }

        // Now we remove all entries that have our key as a prefix and are older than our entry.
        let removed = self.remove_prefix_filtered(entry.key(), |value| entry.value() >= value)?;

        // Insert our new entry.
        self.entry_put(entry)?;
        Ok(InsertOutcome::Inserted { removed })
    }
}

impl<E: RangeEntry, S: Store<E>> Store<E> for &mut S {
    type Error = S::Error;

    type RangeIterator<'a>
        = S::RangeIterator<'a>
    where
        Self: 'a,
        E: 'a;

    type ParentIterator<'a>
        = S::ParentIterator<'a>
    where
        Self: 'a,
        E: 'a;

    fn get_first(&mut self) -> Result<<E as RangeEntry>::Key, Self::Error> {
        (**self).get_first()
    }

    #[cfg(test)]
    fn get(&mut self, key: &<E as RangeEntry>::Key) -> Result<Option<E>, Self::Error> {
        (**self).get(key)
    }

    #[cfg(test)]
    fn len(&mut self) -> Result<usize, Self::Error> {
        (**self).len()
    }

    #[cfg(test)]
    fn is_empty(&mut self) -> Result<bool, Self::Error> {
        (**self).is_empty()
    }

    fn get_fingerprint(
        &mut self,
        range: &Range<<E as RangeEntry>::Key>,
    ) -> Result<Fingerprint, Self::Error> {
        (**self).get_fingerprint(range)
    }

    fn entry_put(&mut self, entry: E) -> Result<(), Self::Error> {
        (**self).entry_put(entry)
    }

    fn get_range(
        &mut self,
        range: Range<<E as RangeEntry>::Key>,
    ) -> Result<Self::RangeIterator<'_>, Self::Error> {
        (**self).get_range(range)
    }

    #[cfg(test)]
    fn prefixed_by(
        &mut self,
        prefix: &<E as RangeEntry>::Key,
    ) -> Result<Self::RangeIterator<'_>, Self::Error> {
        (**self).prefixed_by(prefix)
    }

    fn prefixes_of(
        &mut self,
        key: &<E as RangeEntry>::Key,
    ) -> Result<Self::ParentIterator<'_>, Self::Error> {
        (**self).prefixes_of(key)
    }

    #[cfg(test)]
    fn all(&mut self) -> Result<Self::RangeIterator<'_>, Self::Error> {
        (**self).all()
    }

    #[cfg(test)]
    fn entry_remove(&mut self, key: &<E as RangeEntry>::Key) -> Result<Option<E>, Self::Error> {
        (**self).entry_remove(key)
    }

    fn remove_prefix_filtered(
        &mut self,
        prefix: &<E as RangeEntry>::Key,
        predicate: impl Fn(&<E as RangeEntry>::Value) -> bool,
    ) -> Result<usize, Self::Error> {
        (**self).remove_prefix_filtered(prefix, predicate)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SyncConfig {
    /// Up to how many values to send immediately, before sending only a fingerprint.
    max_set_size: usize,
    /// `k` in the protocol, how many splits to generate. at least 2
    split_factor: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            max_set_size: 1,
            split_factor: 2,
        }
    }
}

/// The outcome of a [`Store::put`] operation.
#[derive(Debug)]
pub(crate) enum InsertOutcome {
    /// The entry was not inserted because a newer entry for its key or a
    /// prefix of its key exists.
    NotInserted,
    /// The entry was inserted.
    Inserted {
        /// Number of entries that were removed as a consequence of this insert operation.
        /// The removed entries had a key that starts with the new entry's key and a lower value.
        removed: usize,
    },
}


