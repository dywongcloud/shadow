//! The signing seam: what a checkpoint is signed WITH, and how a third party checks it.
//!
//! This crate deliberately does not own a key. The platform already has one -- the per-node Ed25519
//! keypair behind [`crate::oplog`]'s sibling `IntegritySigner` in `crates/hive-cloud`, persisted at
//! `$HIVE_DATA/integrity_signing.key` and resolved fail-closed. Introducing a second signing identity
//! would mean a second thing to rotate, a second thing to leak, and no way to tell which one signed a
//! given artefact. So this module defines the SEAM (the trait) and the verifier (which is pure and
//! needs no key at all), and the platform plugs its existing signer into it.
//!
//! Transport identity stays classical Ed25519 -- the same algorithm `IntegritySigner` already uses.
//! Nothing here introduces a new primitive.
//!
//! [`KeyedSigner`] exists for one caller: a process that is not the platform (the witness harness, an
//! offline auditor) and that has been handed a key file explicitly. It FAILS CLOSED: an absent,
//! unreadable, non-hex or unparsable key is an error, never a fresh key nobody has cross-checked --
//! the same discipline `IntegritySigner::open_or_create` applies to a key file that exists.

use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};

/// What a checkpoint signer has to be able to do. Implemented for the platform's existing
/// `IntegritySigner`; nothing in this crate requires that particular type.
pub trait CheckpointSigner: Send + Sync {
    /// Who is signing. Recorded in the artefact so a verifier can attribute a signature and a key
    /// rotation is visible as a changed `(signer_id, public_key)` pair, never as a silently
    /// re-signed history.
    fn signer_id(&self) -> &str;
    /// The raw Ed25519 public key. Carried in the artefact: a signature is only checkable against the
    /// key that made it, and a verifier that has to be told the key out of band can be told the wrong
    /// one.
    fn public_key(&self) -> Vec<u8>;
    /// Sign `msg`. `Err` is a refusal, never a silently-empty signature.
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, String>;
}

/// Why a key could not be loaded. Every variant is a refusal; there is no "generated a fresh one".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// The key file does not exist. Deliberately an error: this crate never mints a signing
    /// identity as a side effect of being asked for one.
    Absent { path: String },
    /// The file exists but could not be read: permissions, or non-UTF-8 content.
    Unreadable { path: String, detail: String },
    /// The content is not hex.
    NotHex { path: String, detail: String },
    /// The bytes are hex but are not a PKCS#8 Ed25519 keypair.
    NotAKey { path: String, detail: String },
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Absent { path } => write!(f, "no signing key at {path}: refusing to mint one"),
            KeyError::Unreadable { path, detail } => write!(f, "{path} could not be read: {detail}"),
            KeyError::NotHex { path, detail } => write!(f, "{path} is not hex: {detail}"),
            KeyError::NotAKey { path, detail } => {
                write!(f, "{path} is not an Ed25519 PKCS#8 keypair: {detail}")
            }
        }
    }
}

impl std::error::Error for KeyError {}

/// A signer built from a PKCS#8 Ed25519 keypair the caller already holds.
pub struct KeyedSigner {
    id: String,
    keypair: Ed25519KeyPair,
    public_key: Vec<u8>,
}

impl KeyedSigner {
    /// Load from a PKCS#8 document that is ALREADY in memory. Fails closed: a malformed document is
    /// refused, never replaced.
    pub fn from_pkcs8(id: &str, pkcs8: &[u8]) -> Result<KeyedSigner, KeyError> {
        let keypair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| KeyError::NotAKey {
            path: format!("in-memory key for {id}"),
            detail: e.to_string(),
        })?;
        let public_key = keypair.public_key().as_ref().to_vec();
        Ok(KeyedSigner {
            id: id.to_string(),
            keypair,
            public_key,
        })
    }

    /// Load from a hex-encoded PKCS#8 file, the on-disk shape the platform's `integrity_signing.key`
    /// already uses. Fails closed on every path except a file that parses.
    pub fn from_hex_file(id: &str, path: &std::path::Path) -> Result<KeyedSigner, KeyError> {
        let p = path.display().to_string();
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(KeyError::Absent { path: p })
            }
            Err(e) => {
                return Err(KeyError::Unreadable {
                    path: p,
                    detail: e.to_string(),
                })
            }
        };
        let bytes = hex::decode(text.trim()).map_err(|e| KeyError::NotHex {
            path: p.clone(),
            detail: e.to_string(),
        })?;
        KeyedSigner::from_pkcs8(id, &bytes).map_err(|e| match e {
            KeyError::NotAKey { detail, .. } => KeyError::NotAKey { path: p, detail },
            other => other,
        })
    }

    /// Generate a fresh PKCS#8 document. Explicit and caller-initiated: nothing in this crate calls
    /// it, and loading never falls back to it. Used by an operator (or a witness harness) that is
    /// deliberately creating a NEW signing identity.
    pub fn generate_pkcs8() -> Result<Vec<u8>, String> {
        let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .map_err(|_| "failed to generate Ed25519 signing key".to_string())?;
        Ok(doc.as_ref().to_vec())
    }
}

impl CheckpointSigner for KeyedSigner {
    fn signer_id(&self) -> &str {
        &self.id
    }

    fn public_key(&self) -> Vec<u8> {
        self.public_key.clone()
    }

    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, String> {
        Ok(self.keypair.sign(msg).as_ref().to_vec())
    }
}

/// Verify an Ed25519 signature against a public key. Pure, keyless, and the reason a third party can
/// check a checkpoint without trusting the node that produced it.
pub fn verify_ed25519(public_key: &[u8], msg: &[u8], signature: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(msg, signature)
        .is_ok()
}
