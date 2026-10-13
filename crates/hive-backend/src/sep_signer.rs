//! Secure-Enclave-resident, per-deployment, non-exportable ECDSA-P256 signing keys (macOS only) for
//! the tamper-evidence chain: a compromised root can still tamper with a running workload but cannot
//! extract the key to forge signatures over an altered history. `security-framework`'s
//! `DataProtectionKeychain` requires the calling binary to be codesigned with a keychain-access-group
//! entitlement (ad-hoc or Developer ID; notarization is not needed) — not yet confirmed by a live
//! run on fleet hardware.

use security_framework::item::{ItemSearchOptions, KeyClass, Location, Reference, SearchResult};
use security_framework::key::{Algorithm, GenerateKeyOptions, KeyType, SecKey, Token};

pub struct SepDeploymentKey {
    pub deployment_id: String,
    pub public_key_der_hex: String,
    pub key_tag: String,
}

fn key_tag(deployment_id: &str) -> String {
    format!("hive.deploy.{deployment_id}")
}

/// Generate a new SEP-resident, non-exportable keypair for `deployment_id` — once, at first build
/// acceptance, so the public key is committed into the chain's very first entry.
pub fn generate_for_deployment(deployment_id: &str) -> anyhow::Result<SepDeploymentKey> {
    let tag = key_tag(deployment_id);
    let mut opts = GenerateKeyOptions::default();
    opts.set_key_type(KeyType::ec_sec_prime_random())
        .set_token(Token::SecureEnclave)
        .set_location(Location::DataProtectionKeychain)
        .set_label(tag.clone());
    let key = SecKey::new(&opts)
        .map_err(|e| anyhow::anyhow!("SecKeyCreateRandomKey (Secure Enclave) failed: {e}"))?;
    let public = key
        .public_key()
        .ok_or_else(|| anyhow::anyhow!("SEP key has no public half (SecKeyCopyPublicKey)"))?;
    let der = public.external_representation().ok_or_else(|| {
        anyhow::anyhow!(
            "SEP public key has no external representation (SecKeyCopyExternalRepresentation)"
        )
    })?;
    Ok(SepDeploymentKey {
        deployment_id: deployment_id.to_string(),
        public_key_der_hex: hex::encode(der.to_vec()),
        key_tag: tag,
    })
}

/// Sign `message` with `deployment_id`'s SEP-resident key, which must already exist (no implicit
/// create-on-sign).
pub fn sign(deployment_id: &str, message: &[u8]) -> anyhow::Result<Vec<u8>> {
    let key = lookup(deployment_id)?
        .ok_or_else(|| anyhow::anyhow!("no SEP key provisioned for deployment {deployment_id}"))?;
    key.create_signature(Algorithm::ECDSASignatureMessageX962SHA256, message)
        .map_err(|e| anyhow::anyhow!("SecKeyCreateSignature failed: {e}"))
}

/// Permanently delete `deployment_id`'s SEP key — genuinely destructive (a new key at the same tag is
/// a NEW key), so callers apply the same blast-radius discipline every GC in this codebase uses.
pub fn delete_for_deployment(deployment_id: &str) -> anyhow::Result<()> {
    let Some(key) = lookup(deployment_id)? else {
        return Ok(()); // already gone — idempotent
    };
    key.delete()
        .map_err(|e| anyhow::anyhow!("SecItemDelete failed for {deployment_id}: {e}"))
}

fn lookup(deployment_id: &str) -> anyhow::Result<Option<SecKey>> {
    let tag = key_tag(deployment_id);
    let results = ItemSearchOptions::new()
        .class(security_framework::item::ItemClass::key())
        .key_class(KeyClass::private())
        .label(&tag)
        .load_refs(true)
        .search();
    let results = match results {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    for r in results {
        if let SearchResult::Ref(Reference::Key(key)) = r {
            return Ok(Some(key));
        }
    }
    Ok(None)
}
