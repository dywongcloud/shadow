//! Per-node signing key for the deployment integrity chain
//! (`hive_core::integrity`). Each node signs only the entries IT authored —
//! this is deliberately NOT a fleet-shared key like `HIVE_SECRET_KEY`
//! (`secrets.rs`): a fleet-shared signing key would make one leaked key
//! forge history for every node, defeating the whole point of attributing
//! each chain entry to the specific node that wrote it. Persistence follows
//! `secrets.rs::load_or_create_key`'s exact discipline (hex-encoded on disk,
//! `chmod 600`, generate-on-first-boot) applied to an asymmetric Ed25519
//! keypair instead of a symmetric AEAD key.
//!
//! Rotation: a superseded public key stays valid for verifying OLD entries
//! forever — signatures are historical facts, never re-signed. Rotating this
//! key only changes which key signs NEW entries going forward (delete
//! `integrity_signing.key` and restart to rotate; the old public key must be
//! kept in the verifier's trust set for as long as any signed history using
//! it is still retained).

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};

pub struct IntegritySigner {
    node: String,
    keypair: Ed25519KeyPair,
    public_key_hex: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct IntegritySignature {
    pub node: String,
    pub public_key_hex: String,
    pub signature_hex: String,
    pub signed_ms: u64,
}

impl IntegritySigner {
    /// Load this node's persisted signing key, or generate and persist a
    /// fresh one on first boot. Never fails silently: an unreadable/corrupt
    /// existing key file is a loud error, not a reason to overwrite history's
    /// signing identity with a fresh key nobody has cross-checked.
    pub fn open_or_create(node: &str) -> anyhow::Result<Self> {
        let path = crate::persist::data_dir().join("integrity_signing.key");
        let pkcs8 = if let Ok(hex_str) = std::fs::read_to_string(&path) {
            hex::decode(hex_str.trim())
                .map_err(|e| anyhow::anyhow!("integrity_signing.key is not valid hex: {e}"))?
        } else {
            let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .map_err(|_| anyhow::anyhow!("failed to generate Ed25519 signing key"))?;
            let bytes = doc.as_ref().to_vec();
            let _ = std::fs::create_dir_all(crate::persist::data_dir());
            std::fs::write(&path, hex::encode(&bytes))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            tracing::info!(path = ?path, "generated per-node integrity signing key");
            bytes
        };
        let keypair = Ed25519KeyPair::from_pkcs8(&pkcs8)
            .map_err(|e| anyhow::anyhow!("integrity_signing.key failed to parse: {e}"))?;
        let public_key_hex = hex::encode(keypair.public_key().as_ref());
        Ok(Self {
            node: node.to_string(),
            keypair,
            public_key_hex,
        })
    }

    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    /// Sign a deployment's current chain head (the value
    /// `hive_core::fold_integrity_chain` returns) — the signature covers the
    /// WHOLE chain up to this point, not one entry, so a verifier checking
    /// the latest signature transitively vouches for every earlier entry
    /// too.
    pub fn sign_chain_head(&self, chain_head_sha256: &str) -> IntegritySignature {
        let sig = self.keypair.sign(chain_head_sha256.as_bytes());
        IntegritySignature {
            node: self.node.clone(),
            public_key_hex: self.public_key_hex.clone(),
            signature_hex: hex::encode(sig.as_ref()),
            signed_ms: hive_core::now_ms(),
        }
    }
}
