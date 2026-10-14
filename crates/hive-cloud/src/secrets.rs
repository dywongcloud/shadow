//! At-rest secret encryption for sensitive project data (env vars marked `sensitive`). Values are
//! sealed with ChaCha20-Poly1305 (AEAD) before they touch disk or the replicated GuardianDB
//! snapshot, so a secret like an API key is never persisted in plaintext: it lives as
//! `enc:v1:<base64>`.
//!
//! Key management (self-hosted, no external KMS), in order:
//! 1. `HIVE_SECRET_KEY` (64 hex chars = 32 bytes), if set (a set-but-EMPTY value counts as set, and
//!    as malformed: it never means "use the file key");
//! 2. `$HIVE_DATA/secret.key` (generated once, persisted, chmod 600);
//! 3. a fresh random key written to that file.
//!
//! Every failure in that resolution is a HARD BOOT ERROR (`load_or_create_key` returns `Err`, and
//! `key()`/`raw_key()` exit the process rather than ever return a key nobody agreed on): a malformed
//! `HIVE_SECRET_KEY` never falls back to the node's own file key (the fleet would silently stop
//! sharing one key), an existing `secret.key` that is not 64 hex is never regenerated over
//! (everything sealed under it would be orphaned), and a key that could not be persisted never runs
//! as a memory-only key (every value sealed this boot would be unreadable after the next restart).
//! Only genuine first-boot absence (no env, no file) creates and persists a fresh key.
//!
//! `decrypt` is a passthrough for non-`enc:v1:` input, so plaintext values written before
//! encryption was added still load (and are re-sealed on the next save).

use std::sync::OnceLock;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

const PREFIX: &str = "enc:v1:";

static KEY: OnceLock<LessSafeKey> = OnceLock::new();

fn key() -> &'static LessSafeKey {
    KEY.get_or_init(|| {
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, raw_key()).expect("valid 32-byte key");
        LessSafeKey::new(unbound)
    })
}

/// The raw 32-byte key material behind [`key`], memoized separately so callers that must DERIVE
/// from it (rather than seal with it) do not re-read the env or the key file: `LessSafeKey`
/// deliberately does not expose its bytes.
fn raw_key() -> &'static [u8; 32] {
    static RAW: OnceLock<[u8; 32]> = OnceLock::new();
    RAW.get_or_init(|| {
        load_or_create_key().unwrap_or_else(|reason| {
            // Loud and deterministic: a key this boot cannot trust or cannot keep is worse than no
            // boot at all (AGENTS.md "fail fast, loud, deterministic" / never weaken a security gate).
            tracing::error!(%reason, "fatal: at-rest secret key resolution failed");
            eprintln!("fatal: at-rest secret key resolution failed: {reason}");
            std::process::exit(1);
        })
    })
}

/// Key material for deriving *other* secrets deterministically (see `zkauth::derive_secret`). One
/// fleet-shared key (`HIVE_SECRET_KEY`) is what lets every node derive an identical value for the
/// same input, so derived data survives a request landing on any node.
pub(crate) fn key_material() -> [u8; 32] {
    *raw_key()
}

/// Keys no longer used for SEALING that must still OPEN previously sealed data:
/// `HIVE_SECRET_KEY_OLD` (comma-separated hex, for a deliberate rotation) and the persisted
/// `secret.key` file.
///
/// `load_or_create_key` prefers `HIVE_SECRET_KEY` and falls back to the file only when it is unset,
/// so introducing the env var on a running node would otherwise leave every value sealed under the
/// file key undecryptable, and since `decrypt` returns its input unchanged on AEAD failure the
/// damage surfaces as garbage `enc:v1:...` strings handed to callers, not as an error. Keeping
/// superseded keys as read-only openers makes rotation non-destructive: old data still reads, and
/// because `encrypt` always seals with the CURRENT key each value re-seals forward the next time it
/// is written.
fn legacy_keys() -> &'static Vec<[u8; 32]> {
    static LEGACY: OnceLock<Vec<[u8; 32]>> = OnceLock::new();
    LEGACY.get_or_init(|| {
        let mut keys: Vec<[u8; 32]> = Vec::new();
        let mut push = |k: [u8; 32]| {
            if k != *raw_key() && !keys.contains(&k) {
                keys.push(k);
            }
        };
        if let Ok(list) = std::env::var("HIVE_SECRET_KEY_OLD") {
            for part in list.split(',') {
                if let Some(k) = hex_to_32(part) {
                    push(k);
                }
            }
        }
        if let Ok(s) = std::fs::read_to_string(crate::persist::data_dir().join("secret.key")) {
            if let Some(k) = hex_to_32(&s) {
                push(k);
            }
        }
        keys
    })
}

/// Opens a sealed value, `None` when it genuinely cannot be opened. [`decrypt`] must stay lossy (it
/// returns its input unchanged so values written before encryption existed still load), which makes
/// "this is plaintext" and "this is ciphertext I cannot open" indistinguishable to its callers;
/// anything that must TELL THE DIFFERENCE (an audit, a health check, a migration) uses this
/// instead.
pub(crate) fn try_decrypt(s: &str) -> Option<String> {
    let b64 = s.strip_prefix(PREFIX)?;
    let raw = B64.decode(b64).ok()?;
    if raw.len() <= NONCE_LEN {
        return None;
    }
    let (nonce_b, ct) = raw.split_at(NONCE_LEN);
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(nonce_b);
    for opener in std::iter::once(key()).chain(legacy_openers()) {
        let mut buf = ct.to_vec();
        if let Ok(pt) =
            opener.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        {
            return Some(String::from_utf8_lossy(pt).into_owned());
        }
    }
    None
}

/// Boot-time at-rest audit: how many sealed values in this node's top-level `*.json` data files can
/// no longer be opened by ANY key we hold. The failure it detects is otherwise invisible: a key
/// change orphans sealed data, `decrypt` hands the raw ciphertext back instead of failing, and the
/// app just receives a nonsense secret with no log line, error or health-check change. Counting the
/// unopenable values at startup turns that into one loud line an operator can act on.
pub(crate) fn audit_at_rest() {
    let dir = crate::persist::data_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let (mut sealed, mut broken) = (0usize, 0usize);
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (i, _) in raw.match_indices(PREFIX) {
            let tail = &raw[i..];
            let end = tail
                .char_indices()
                .skip(PREFIX.len())
                .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '='))
                .map(|(j, _)| j)
                .unwrap_or(tail.len());
            sealed += 1;
            if try_decrypt(&tail[..end]).is_none() {
                broken += 1;
            }
        }
    }
    if broken > 0 {
        tracing::error!(
            sealed,
            broken,
            dir = ?dir,
            "at-rest secrets cannot be decrypted with any configured key — they were sealed under a \
             superseded key. Set HIVE_SECRET_KEY_OLD to the previous key (comma-separated hex) so they \
             can be read and re-sealed; until then these values are served as raw ciphertext."
        );
    } else if sealed > 0 {
        tracing::info!(sealed, "at-rest secrets: all sealed values open cleanly");
    }
}

/// Prepared AEAD openers for every superseded key, built once.
fn legacy_openers() -> impl Iterator<Item = &'static LessSafeKey> {
    static OPENERS: OnceLock<Vec<LessSafeKey>> = OnceLock::new();
    OPENERS
        .get_or_init(|| {
            legacy_keys()
                .iter()
                .filter_map(|k| {
                    UnboundKey::new(&CHACHA20_POLY1305, k)
                        .ok()
                        .map(LessSafeKey::new)
                })
                .collect()
        })
        .iter()
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

/// Key resolution, in order: `HIVE_SECRET_KEY` (64 hex chars); else `$HIVE_DATA/secret.key`; else a
/// fresh random key written to that file (chmod 600 on unix). Every branch that cannot produce a key
/// everyone will agree on, or cannot persist one it just made, is `Err` (see the module doc) — the
/// dev flow (no env, no file) is the only path that succeeds by creating state.
fn load_or_create_key() -> Result<[u8; 32], String> {
    let data_dir = crate::persist::data_dir();
    if let Ok(h) = std::env::var("HIVE_SECRET_KEY") {
        return hex_to_32(&h).ok_or_else(|| {
            if h.trim().is_empty() {
                // A unit/drop-in line `Environment=HIVE_SECRET_KEY=` (or an empty/blank value) SETS
                // the variable: it is not "absent", so it can never mean "use the file key".
                format!(
                    "HIVE_SECRET_KEY is set but EMPTY (a `HIVE_SECRET_KEY=` line with no value) — \
                     unset it, or give it exactly 64 hex characters; refusing to fall back to this \
                     node's own {}/secret.key, which the rest of the fleet does not share",
                    data_dir.display()
                )
            } else {
                format!(
                    "HIVE_SECRET_KEY is set but is not 64 hex characters ({} chars) — refusing to \
                     fall back to this node's own {}/secret.key, which the rest of the fleet does \
                     not share",
                    h.trim().len(),
                    data_dir.display()
                )
            }
        });
    }
    let path = data_dir.join("secret.key");
    match std::fs::read_to_string(&path) {
        Ok(s) => hex_to_32(&s).ok_or_else(|| {
            format!(
                "{} exists but is not 64 hex characters ({} bytes) — refusing to regenerate over it \
                 (every value already sealed under it would be orphaned); restore the correct key or \
                 remove the file to mint a fresh one",
                path.display(),
                s.trim().len()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut k = [0u8; 32];
            SystemRandom::new()
                .fill(&mut k)
                .map_err(|_| "system RNG failed while generating a new secret key".to_string())?;
            std::fs::create_dir_all(&data_dir)
                .map_err(|e| format!("could not create {}: {e}", data_dir.display()))?;
            std::fs::write(&path, hex_encode(&k)).map_err(|e| {
                format!(
                    "could not persist a new key to {}: {e} — refusing to run on a memory-only key \
                     (everything sealed this boot would be unreadable after the next restart)",
                    path.display()
                )
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| format!("could not chmod 600 {}: {e}", path.display()))?;
            }
            tracing::info!(path = ?path, "generated at-rest secret key for env-var encryption");
            Ok(k)
        }
        Err(e) => Err(format!(
            "could not read {}: {e} — refusing to treat an existing key file as absent and replace it",
            path.display()
        )),
    }
}

/// Has this value already been sealed?
pub fn is_encrypted(s: &str) -> bool {
    s.starts_with(PREFIX)
}

/// Seal a plaintext value → `enc:v1:<base64(nonce||ciphertext||tag)>`.
/// Returns the input unchanged if it's empty or already sealed.
///
/// A failure of the system RNG or of the seal returns the plaintext unchanged (fail open: the value
/// is stored readable rather than lost).
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
///
/// The current key is tried first, then every superseded key: without that fallback, introducing
/// `HIVE_SECRET_KEY` on a node that had been using its persisted `secret.key` silently orphaned
/// everything sealed beforehand, and callers received the raw `enc:v1:...` ciphertext as if it were
/// the secret. Use [`try_decrypt`] to tell "plaintext" from "ciphertext no key opens".
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
    for opener in std::iter::once(key()).chain(legacy_openers()) {
        let mut buf = ct.to_vec();
        if let Ok(pt) =
            opener.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        {
            return String::from_utf8_lossy(pt).into_owned();
        }
    }
    s.to_string()
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

#[cfg(test)]
mod tests_ext {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip_and_passthrough() {
        let sealed = encrypt("hunter2");
        assert!(is_encrypted(&sealed));
        assert_ne!(sealed, "hunter2");
        assert_eq!(decrypt(&sealed), "hunter2");
        // Non-sealed input is passthrough (back-compat) and not "encrypted".
        assert!(!is_encrypted("plain"));
        assert_eq!(decrypt("plain"), "plain");
        // Empty stays empty (never sealed).
        assert_eq!(encrypt(""), "");
    }

    #[test]
    fn double_encrypt_is_noop() {
        let once = encrypt("x");
        let twice = encrypt(&once);
        assert_eq!(once, twice, "already-sealed values are not re-sealed");
        assert_eq!(decrypt(&twice), "x");
    }

    #[test]
    fn distinct_nonces_produce_distinct_ciphertexts() {
        // Same plaintext, different sealed blobs (random nonce), both decrypt back.
        let a = encrypt("same");
        let b = encrypt("same");
        assert_ne!(a, b);
        assert_eq!(decrypt(&a), "same");
        assert_eq!(decrypt(&b), "same");
    }
}
