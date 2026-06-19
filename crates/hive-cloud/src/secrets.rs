//! At-rest secret encryption for sensitive project data (env vars marked
//! `sensitive`). Values are sealed with ChaCha20-Poly1305 (AEAD) before they
//! ever touch disk or the replicated GuardianDB snapshot, so a secret like an
//! API key is never persisted in plaintext — it lives as `enc:v1:<base64>`.
//!
//! Key management (self-hosted, no external KMS):
//!   1. `HIVE_SECRET_KEY` (64 hex chars = 32 bytes), if set; else
//!   2. `$HIVE_DATA/secret.key` (generated once, persisted, chmod 600); else
//!   3. a fresh random key written to that file.
//!
//! `decrypt` is a no-op passthrough for non-`enc:v1:` input, so plaintext values
//! written before encryption was added still load (and get re-sealed on next
//! save).

use std::sync::OnceLock;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

const PREFIX: &str = "enc:v1:";

static KEY: OnceLock<LessSafeKey> = OnceLock::new();

fn key() -> &'static LessSafeKey {
    KEY.get_or_init(|| {
        let raw = load_or_create_key();
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, &raw).expect("valid 32-byte key");
        LessSafeKey::new(unbound)
    })
}

fn hex_to_32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn load_or_create_key() -> [u8; 32] {
    if let Ok(h) = std::env::var("HIVE_SECRET_KEY") {
        if let Some(k) = hex_to_32(&h) {
            return k;
        }
        tracing::warn!("HIVE_SECRET_KEY is not 64 hex chars — ignoring");
    }
    let path = crate::persist::data_dir().join("secret.key");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Some(k) = hex_to_32(&s) {
            return k;
        }
    }
    // Generate and persist a fresh key (chmod 600 on unix).
    let mut k = [0u8; 32];
    SystemRandom::new().fill(&mut k).expect("system RNG");
    let _ = std::fs::create_dir_all(crate::persist::data_dir());
    if std::fs::write(&path, hex_encode(&k)).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!(path = ?path, "generated at-rest secret key for env-var encryption");
    }
    k
}

/// Has this value already been sealed?
pub fn is_encrypted(s: &str) -> bool {
    s.starts_with(PREFIX)
}

/// Seal a plaintext value → `enc:v1:<base64(nonce||ciphertext||tag)>`.
/// Returns the input unchanged if it's empty or already sealed.
pub fn encrypt(plaintext: &str) -> String {
    if plaintext.is_empty() || is_encrypted(plaintext) {
        return plaintext.to_string();
    }
    let mut nonce = [0u8; NONCE_LEN];
    if SystemRandom::new().fill(&mut nonce).is_err() {
        return plaintext.to_string();
    }
    let mut buf = plaintext.as_bytes().to_vec();
    if key()
        .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        .is_err()
    {
        return plaintext.to_string();
    }
    let mut blob = Vec::with_capacity(NONCE_LEN + buf.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&buf);
    format!("{PREFIX}{}", B64.encode(blob))
}

/// Open a sealed value. Passthrough for anything not `enc:v1:` (backward compat).
pub fn decrypt(s: &str) -> String {
    let Some(b64) = s.strip_prefix(PREFIX) else {
        return s.to_string();
    };
    let Ok(raw) = B64.decode(b64) else {
        return s.to_string();
    };
    if raw.len() <= NONCE_LEN {
        return s.to_string();
    }
    let (nonce_b, ct) = raw.split_at(NONCE_LEN);
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(nonce_b);
    let mut buf = ct.to_vec();
    match key().open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf) {
        Ok(pt) => String::from_utf8_lossy(pt).into_owned(),
        Err(_) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_opaque() {
        std::env::set_var(
            "HIVE_SECRET_KEY",
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        );
        let secret = "super-secret-token-123";
        let sealed = encrypt(secret);
        assert!(is_encrypted(&sealed));
        assert!(!sealed.contains(secret)); // ciphertext doesn't leak plaintext
        assert_eq!(decrypt(&sealed), secret);
        // passthrough for plaintext / empty
        assert_eq!(decrypt("plain"), "plain");
        assert_eq!(encrypt(""), "");
        // double-encrypt is a no-op
        assert_eq!(encrypt(&sealed), sealed);
    }
}
