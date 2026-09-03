//! Secure-Enclave-resident, per-deployment, non-exportable ECDSA-P256 signing
//! keys — macOS only. Each tenant deployment running on this node gets its
//! own keypair whose PRIVATE key never leaves the physical SEP coprocessor,
//! used to sign that deployment's entries in the tamper-evidence chain
//! (`hive_core::integrity`). A compromised root user on this box can still
//! tamper with a *running* workload, but cannot extract this key to
//! retroactively forge a legitimate-looking signature over an altered
//! history it already signed — a categorically stronger guarantee than the
//! Linux-node software signing key (`hive-cloud::integrity_signer`) gets for
//! that one specific failure mode. See `hive_core::integrity`'s module doc
//! for the full honest scope of what this platform's tamper-evidence design
//! does and does not claim.
//!
//! **Code-signing requirement — confirmed, not merely suspected.** Reading
//! `security-framework` 3.7.0's own source (`src/item.rs`,
//! `Location::DataProtectionKeychain`'s doc comment) confirms directly:
//! "Keys stored in the Secure Enclave _must_ use this keychain... This
//! keychain requires the calling binary to be codesigned with entitlements
//! for the `KeychainAccessGroups` it is supposed to access." This resolves
//! the conflicting precedent found during design research (some SEP paths
//! reportedly worked from an unsigned CLI tool) in the more conservative
//! direction FOR THIS SPECIFIC API SURFACE: `hive-cloud`'s macOS binary
//! needs, at minimum, an ad-hoc or Developer ID code signature with a
//! keychain-access-group entitlement before `generate_for_deployment` will
//! succeed — full App Store notarization is NOT required (notarization
//! gates Gatekeeper on distribution outside the machine that built it; a
//! fleet-internal binary run via launchd on a machine already under this
//! operator's control never goes through Gatekeeper). This has not yet been
//! confirmed by an actual live run on fleet hardware — see
//! `sep-signer-macos-module`'s PRD row, blocked on live Mac-node access.

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

/// Generate a new SEP-resident, non-exportable keypair for `deployment_id`.
/// Called once, at first build acceptance for that deployment on this node
/// (same hook as the chain's `BuildAccepted` entry) — never at first
/// execution, so the public key is committed into the chain's very first
/// entry and a verifier never sees entries with no key to check them
/// against.
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
        anyhow::anyhow!("SEP public key has no external representation (SecKeyCopyExternalRepresentation)")
    })?;
    Ok(SepDeploymentKey {
        deployment_id: deployment_id.to_string(),
        public_key_der_hex: hex::encode(der.to_vec()),
        key_tag: tag,
    })
}

/// Sign `message` with `deployment_id`'s SEP-resident private key. The key
/// must already exist (`generate_for_deployment` must have run for this
/// deployment on this node) — this function does not create one, since a
/// silent implicit-create-on-sign would make "which key signed this" opaque
/// to the caller.
pub fn sign(deployment_id: &str, message: &[u8]) -> anyhow::Result<Vec<u8>> {
    let key = lookup(deployment_id)?
        .ok_or_else(|| anyhow::anyhow!("no SEP key provisioned for deployment {deployment_id}"))?;
    key.create_signature(Algorithm::ECDSASignatureMessageX962SHA256, message)
        .map_err(|e| anyhow::anyhow!("SecKeyCreateSignature failed: {e}"))
}

/// Permanently delete `deployment_id`'s SEP key. **This is genuinely
/// destructive in a way file-based GC is not**: a deleted SEP key can never
/// be regenerated identically (a new key at the same tag is a NEW key, not
/// a recovery), so callers must apply the same blast-radius discipline
/// every GC in this codebase already uses (empty-keep-set refuses, a grace
/// window, a max-reap-fraction guard — see `browser_artifacts::gc`'s
/// template) before ever calling this.
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
        // errSecItemNotFound surfaces as a generic search error in this
        // crate's Result type — no distinct "not found" variant to match
        // on, so any search failure here means "no such key" rather than a
        // deeper fault (a real fault would already have failed
        // generate_for_deployment earlier, which is the only writer).
        Err(_) => return Ok(None),
    };
    for r in results {
        if let SearchResult::Ref(Reference::Key(key)) = r {
            return Ok(Some(key));
        }
    }
    Ok(None)
}
