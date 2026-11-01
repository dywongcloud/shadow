//! Merkle commitments over one window of log entries: the inclusion-proof primitive.
//!
//! The oplog is hash-chained, so "entry X is in the log" is provable by walking the chain -- but that
//! walk costs one hash per entry between X and the head, and the whole point of a compact artefact is
//! that its size does not grow with the log. A binary commitment over the window's entry hashes gives
//! an O(log n) proof instead: a verifier holding the signed window root and `log2(window)` siblings
//! recomputes the root itself.
//!
//! The tree is a carry-up fold, not a padded power-of-two tree, because a window's entry count is
//! whatever it is:
//!
//! ```text
//! level 0: L0 L1 L2 L3 L4      (leaves, one per entry hash)
//! level 1: N01 N23 L4          (odd tail carries up unchanged)
//! level 2: N0123 L4
//! root:    N01234
//! ```
//!
//! Which sibling a leaf has -- left, right, or none because it carried -- is a function of
//! `(index, len)` at each level, so it is DERIVED by the verifier from the window's leaf count and
//! never stored. A proof therefore cannot disagree with the tree shape it was cut from.
//!
//! BLAKE3, domain-separated per level, per the repo's new-content-identity rule. Leaf and internal
//! nodes use different domains so a leaf can never be confused for an internal node.

/// Domain for a leaf: `BLAKE3(LEAF_DOMAIN || entry_hash)`.
pub const LEAF_DOMAIN: &[u8] = b"hive-contract/merkle/leaf/v1";
/// Domain for an internal node: `BLAKE3(NODE_DOMAIN || left || right)`.
pub const NODE_DOMAIN: &[u8] = b"hive-contract/merkle/node/v1";
/// Domain for the empty-window root.
pub const EMPTY_DOMAIN: &[u8] = b"hive-contract/merkle/empty/v1";

/// The root of an EMPTY window. A distinct constant, never the hash of nothing: a window with no
/// entries must not produce the same root as any window with entries, or "this window is empty" would
/// be indistinguishable from "this window has one entry".
pub fn empty_root() -> String {
    fluid_core::content_hash(EMPTY_DOMAIN, b"")
}

/// One leaf: commits to an entry's own chain hash, so the tree covers the log's content identity and
/// never a second, parallel history.
pub fn leaf_hash(entry_hash: &str) -> String {
    fluid_core::content_hash(LEAF_DOMAIN, entry_hash.as_bytes())
}

/// One internal node. Length-prefixed by the domains being fixed-width: both children are 64-char
/// hex, so `left || right` is unambiguous without a separator.
pub fn node_hash(left: &str, right: &str) -> String {
    let mut pre = left.as_bytes().to_vec();
    pre.extend_from_slice(right.as_bytes());
    fluid_core::content_hash(NODE_DOMAIN, &pre)
}

/// Fold one level into the next, carrying an odd tail up unchanged.
fn fold(level: Vec<String>) -> Vec<String> {
    if level.len() <= 1 {
        return level;
    }
    let mut next = Vec::with_capacity((level.len() + 1) / 2);
    let mut i = 0usize;
    while i + 1 < level.len() {
        next.push(node_hash(&level[i], &level[i + 1]));
        i += 2;
    }
    if i < level.len() {
        // Odd tail: carries up with no sibling. Deterministic, and the verifier derives it from the
        // level's own length.
        next.push(level[i].clone());
    }
    next
}

/// The window's commitment: the root of the carry-up tree over its entry hashes.
///
/// `empty_root()` for a window with no entries, never a hash of an empty string.
pub fn merkle_root(entry_hashes: &[String]) -> String {
    if entry_hashes.is_empty() {
        return empty_root();
    }
    let mut level: Vec<String> = entry_hashes.iter().map(|h| leaf_hash(h)).collect();
    while level.len() > 1 {
        level = fold(level);
    }
    level.pop().unwrap_or_else(empty_root)
}

/// A sibling in an inclusion path. Which side it sits on is what makes the fold order unambiguous.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Sibling {
    Left(String),
    Right(String),
}

/// A compact inclusion proof: "the entry whose hash is [`MerklePath::leaf`] is the entry at
/// [`MerklePath::index`] of a window of [`MerklePath::leaves`] entries whose root is X".
///
/// Size is `log2(leaves)` siblings, so it stays small as the window grows -- that is the whole reason
/// this exists. Carried-up levels contribute no sibling.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MerklePath {
    /// Position of the entry within the window, `0..leaves`.
    pub index: u64,
    /// The number of entries in the window. Carried in the proof so a verifier never has to trust a
    /// count it was handed separately; it is checked by reaching the root.
    pub leaves: u64,
    /// The entry's own chain hash.
    pub leaf: String,
    pub siblings: Vec<Sibling>,
}

/// Why a path could not be cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofError {
    /// `index` is not inside the window.
    OutOfRange { index: u64, leaves: u64 },
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProofError::OutOfRange { index, leaves } => {
                write!(f, "index {index} is outside a window of {leaves} entries")
            }
        }
    }
}

/// Cut an inclusion path for the entry at `index` in a window of `entry_hashes`.
pub fn merkle_prove(entry_hashes: &[String], index: u64) -> Result<MerklePath, ProofError> {
    let i0 = index as usize;
    if i0 >= entry_hashes.len() {
        return Err(ProofError::OutOfRange {
            index,
            leaves: entry_hashes.len() as u64,
        });
    }
    let mut level: Vec<String> = entry_hashes.iter().map(|h| leaf_hash(h)).collect();
    let mut i = i0;
    let mut siblings = Vec::new();
    while level.len() > 1 {
        let n = level.len();
        if i % 2 == 1 {
            siblings.push(Sibling::Left(level[i - 1].clone()));
        } else if i + 1 < n {
            siblings.push(Sibling::Right(level[i + 1].clone()));
        }
        // else: i is the last index of an odd-length level -- it carries up with no sibling.
        level = fold(level);
        i /= 2;
    }
    Ok(MerklePath {
        index,
        leaves: entry_hashes.len() as u64,
        leaf: entry_hashes[i0].clone(),
        siblings,
    })
}

/// Why a path did not verify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The path's index is outside the window it claims.
    OutOfRange { index: u64, leaves: u64 },
    /// The path carries a sibling count the tree shape does not produce: not cut from this shape, or
    /// cut against a different leaf count.
    ShapeMismatch { used: usize, carried: usize },
    /// The recomputed root is not the root the path was offered against: the entry is not in the
    /// window this root commits to.
    RootMismatch { computed: String, expected: String },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::OutOfRange { index, leaves } => {
                write!(f, "path index {index} is outside a window of {leaves} entries")
            }
            VerifyError::ShapeMismatch { used, carried } => write!(
                f,
                "path shape mismatch: fold consumed {used} of {carried} siblings"
            ),
            VerifyError::RootMismatch { computed, expected } => write!(
                f,
                "path recomputes to {computed}, offered root is {expected}"
            ),
        }
    }
}

/// Recompute the root from a path and compare it to `root`.
///
/// The tree shape comes from [`MerklePath::leaves`] alone -- the same derivation [`merkle_prove`]
/// used -- so a path cut against one leaf count cannot be made to verify against another, and a
/// sibling left over at the end fails rather than being ignored.
pub fn merkle_verify(path: &MerklePath, root: &str) -> Result<(), VerifyError> {
    if path.leaves == 0 || path.index >= path.leaves {
        return Err(VerifyError::OutOfRange {
            index: path.index,
            leaves: path.leaves,
        });
    }
    let mut h = leaf_hash(&path.leaf);
    let mut i = path.index as usize;
    let mut n = path.leaves as usize;
    let mut used = 0usize;
    while n > 1 {
        if i % 2 == 1 {
            match path.siblings.get(used) {
                Some(Sibling::Left(s)) => h = node_hash(s, &h),
                _ => {
                    return Err(VerifyError::ShapeMismatch {
                        used,
                        carried: path.siblings.len(),
                    })
                }
            }
            used += 1;
        } else if i + 1 < n {
            match path.siblings.get(used) {
                Some(Sibling::Right(s)) => h = node_hash(&h, s),
                _ => {
                    return Err(VerifyError::ShapeMismatch {
                        used,
                        carried: path.siblings.len(),
                    })
                }
            }
            used += 1;
        }
        // else: carried up, no sibling consumed.
        n = (n + 1) / 2;
        i /= 2;
    }
    if used != path.siblings.len() {
        return Err(VerifyError::ShapeMismatch {
            used,
            carried: path.siblings.len(),
        });
    }
    if h != root {
        return Err(VerifyError::RootMismatch {
            computed: h,
            expected: root.to_string(),
        });
    }
    Ok(())
}
