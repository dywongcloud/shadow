//! Native implementation of the `pgcrypto` extension.
//!
//! PostgreSQL's pgcrypto delegates to OpenSSL; GuardianDB implements the same
//! surface on pure-Rust cryptography (the RustCrypto `md-5`/`sha1`/`sha2`/
//! `hmac` crates plus `bcrypt`):
//!
//! * `digest(data, type)` / `hmac(data, key, type)` — MD5, SHA-1 and the
//!   SHA-2 family, returning `bytea`.
//! * `encode(bytea, format)` / `decode(text, format)` — `hex`, `base64` and
//!   `escape` conversions between binary and text.
//! * `gen_random_bytes(count)` / `gen_random_uuid()` — OS-sourced,
//!   cryptographically secure randomness (no userspace PRNG fallback).
//! * `gen_salt(type [, iter])` / `crypt(password, salt)` — password hashing.
//!   Only the Blowfish scheme (`bf`, i.e. `$2a$`/`$2b$`/`$2y$` bcrypt) is
//!   implemented; the obsolete DES/XDES/MD5 crypt schemes are refused with a
//!   typed error rather than silently producing weak hashes. As in
//!   PostgreSQL, `crypt(pw, stored_hash) = stored_hash` checks a password,
//!   because re-crypting with the stored hash as the salt reproduces it.
//!
//! All functions are strict: any SQL NULL argument yields NULL.

use super::{
    ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_bytes, arg_i64, arg_text, no_such,
};
use crate::relational::SqlValue;
use crate::sql::error::{Result, SqlError};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use md5::Md5;
use rand::TryRng;
use rand::rngs::SysRng;
use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

pub static DEF: ExtensionDef = ExtensionDef {
    name: "pgcrypto",
    default_version: "1.3",
    comment: "cryptographic functions",
    requires: &[],
    functions: &[
        "crypt",
        "decode",
        "digest",
        "encode",
        "gen_random_bytes",
        "gen_random_uuid",
        "gen_salt",
        "hmac",
    ],
    types: &[],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

fn call(_ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    // Every pgcrypto function is strict: NULL in, NULL out.
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    match name {
        "digest" => {
            let data = arg_bytes(args, 0, "digest")?;
            let algorithm = arg_text(args, 1, "digest")?;
            Ok(SqlValue::Bytea(hash_bytes(&algorithm, &data)?))
        }
        "hmac" => {
            let data = arg_bytes(args, 0, "hmac")?;
            let key = arg_bytes(args, 1, "hmac")?;
            let algorithm = arg_text(args, 2, "hmac")?;
            Ok(SqlValue::Bytea(hmac_bytes(&algorithm, &key, &data)?))
        }
        "gen_random_bytes" => {
            let count = arg_i64(args, 0, "gen_random_bytes")?;
            if !(1..=1024).contains(&count) {
                return Err(SqlError::InvalidParameter(
                    "Length not in range 1..1024".into(),
                ));
            }
            let mut buf = vec![0u8; count as usize];
            os_random(&mut buf)?;
            Ok(SqlValue::Bytea(buf))
        }
        "gen_random_uuid" => Ok(SqlValue::Uuid(uuid::Uuid::new_v4())),
        "encode" => {
            let data = arg_bytes(args, 0, "encode")?;
            let format = arg_text(args, 1, "encode")?;
            Ok(SqlValue::Text(encode_bytes(&data, &format)?))
        }
        "decode" => {
            let input = arg_text(args, 0, "decode")?;
            let format = arg_text(args, 1, "decode")?;
            Ok(SqlValue::Bytea(decode_text(&input, &format)?))
        }
        "gen_salt" => gen_salt(args),
        "crypt" => crypt(args),
        _ => Err(no_such(name)),
    }
}

// ---------------------------------------------------------------- digests --

/// PostgreSQL's error for a `type` string that names no digest.
fn no_such_algorithm(algorithm: &str) -> SqlError {
    SqlError::InvalidParameter(format!(
        "Cannot use \"{algorithm}\": No such hash algorithm"
    ))
}

fn hash_bytes(algorithm: &str, data: &[u8]) -> Result<Vec<u8>> {
    Ok(match algorithm.to_ascii_lowercase().as_str() {
        "md5" => Md5::digest(data).to_vec(),
        "sha1" => Sha1::digest(data).to_vec(),
        "sha224" => Sha224::digest(data).to_vec(),
        "sha256" => Sha256::digest(data).to_vec(),
        "sha384" => Sha384::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        _ => return Err(no_such_algorithm(algorithm)),
    })
}

fn hmac_bytes(algorithm: &str, key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    macro_rules! compute {
        ($digest:ty) => {{
            let mut mac =
                <Hmac<$digest>>::new_from_slice(key).expect("HMAC accepts keys of any length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }};
    }
    Ok(match algorithm.to_ascii_lowercase().as_str() {
        "md5" => compute!(Md5),
        "sha1" => compute!(Sha1),
        "sha224" => compute!(Sha224),
        "sha256" => compute!(Sha256),
        "sha384" => compute!(Sha384),
        "sha512" => compute!(Sha512),
        _ => return Err(no_such_algorithm(algorithm)),
    })
}

// ----------------------------------------------------------------- random --

/// Fill `buf` from the operating-system CSPRNG (`rand`'s `SysRng`, backed by
/// `getrandom`). pgcrypto promises cryptographically strong randomness, so no
/// userspace PRNG is substituted on failure.
fn os_random(buf: &mut [u8]) -> Result<()> {
    SysRng
        .try_fill_bytes(buf)
        .map_err(|e| SqlError::Internal(format!("system random source failed: {e}")))
}

// -------------------------------------------------------- encode / decode --

fn unknown_encoding(format: &str) -> SqlError {
    SqlError::InvalidParameter(format!("unrecognized encoding: \"{format}\""))
}

fn encode_bytes(data: &[u8], format: &str) -> Result<String> {
    match format.to_ascii_lowercase().as_str() {
        "hex" => Ok(hex::encode(data)),
        "base64" => Ok(BASE64_STANDARD.encode(data)),
        "escape" => Ok(escape_encode(data)),
        _ => Err(unknown_encoding(format)),
    }
}

fn decode_text(input: &str, format: &str) -> Result<Vec<u8>> {
    let invalid = || SqlError::InvalidTextRepresentation {
        ty: format!("{format} data"),
        value: input.to_string(),
    };
    match format.to_ascii_lowercase().as_str() {
        "hex" => hex_decode(input).ok_or_else(invalid),
        "base64" => {
            // PostgreSQL ignores whitespace in base64 input (its encoder
            // wraps lines); strip it before strict decoding.
            let compact: String = input.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            BASE64_STANDARD
                .decode(compact.as_bytes())
                .map_err(|_| invalid())
        }
        "escape" => escape_decode(input).ok_or_else(invalid),
        _ => Err(unknown_encoding(format)),
    }
}

/// `encode(..., 'escape')`: printable ASCII passes through unchanged, a
/// backslash doubles to `\\`, and every other byte (NUL, control bytes,
/// high-bit bytes) becomes a three-digit octal escape `\nnn`. High-bit bytes
/// must be escaped for the result to be valid text.
fn escape_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len());
    for &b in data {
        match b {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x5b | 0x5d..=0x7f => out.push(char::from(b)),
            _ => {
                out.push('\\');
                out.push(char::from(b'0' + (b >> 6)));
                out.push(char::from(b'0' + ((b >> 3) & 7)));
                out.push(char::from(b'0' + (b & 7)));
            }
        }
    }
    out
}

/// Inverse of [`escape_encode`]: `\\` is a backslash, `\nnn` (first digit
/// 0–3, like PostgreSQL) is an octal byte, anything else passes through.
/// `None` marks a malformed escape sequence.
fn escape_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
        } else if bytes.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else if i + 3 < bytes.len()
            && (b'0'..=b'3').contains(&bytes[i + 1])
            && (b'0'..=b'7').contains(&bytes[i + 2])
            && (b'0'..=b'7').contains(&bytes[i + 3])
        {
            out.push(
                ((bytes[i + 1] - b'0') << 6) | ((bytes[i + 2] - b'0') << 3) | (bytes[i + 3] - b'0'),
            );
            i += 4;
        } else {
            return None;
        }
    }
    Some(out)
}

/// Hex decoding that skips whitespace between (but not inside) byte pairs,
/// matching PostgreSQL. `None` marks a bad digit or an odd digit count.
fn hex_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 2);
    let mut pending: Option<u8> = None;
    for b in input.bytes() {
        if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
            continue;
        }
        let nibble = char::from(b).to_digit(16)? as u8;
        match pending.take() {
            None => pending = Some(nibble),
            Some(high) => out.push((high << 4) | nibble),
        }
    }
    if pending.is_some() {
        return None; // Odd number of hex digits.
    }
    Some(out)
}

// -------------------------------------------------------------- passwords --

/// The traditional crypt/bcrypt base64 alphabet (not the RFC 4648 order).
const BCRYPT_ALPHABET: &[u8; 64] =
    b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Encode 16 salt bytes as bcrypt's 22-character unpadded base64.
fn bcrypt_b64_encode(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(22);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(char::from(BCRYPT_ALPHABET[((acc >> bits) & 0x3f) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(
            BCRYPT_ALPHABET[((acc << (6 - bits)) & 0x3f) as usize],
        ));
    }
    out
}

/// Decode 22 bcrypt-base64 characters back to the 16 salt bytes. The 4
/// trailing bits of the final character are ignored, as in crypt_blowfish.
/// `None` marks a wrong length or a character outside the alphabet.
fn bcrypt_b64_decode(salt: &str) -> Option<[u8; 16]> {
    let chars = salt.as_bytes();
    if chars.len() != 22 {
        return None;
    }
    let mut out = [0u8; 16];
    let mut n = 0usize;
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in chars {
        let value = BCRYPT_ALPHABET.iter().position(|&a| a == c)? as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 && n < out.len() {
            bits -= 8;
            out[n] = ((acc >> bits) & 0xff) as u8;
            n += 1;
        }
    }
    (n == out.len()).then_some(out)
}

fn gen_salt(args: &[SqlValue]) -> Result<SqlValue> {
    let ty = arg_text(args, 0, "gen_salt")?;
    let cost = if args.len() >= 2 {
        arg_i64(args, 1, "gen_salt")?
    } else {
        6 // PostgreSQL's default Blowfish iteration count.
    };
    match ty.to_ascii_lowercase().as_str() {
        "bf" => {
            if !(4..=31).contains(&cost) {
                return Err(SqlError::InvalidParameter(
                    "gen_salt: Incorrect number of rounds".into(),
                ));
            }
            let mut salt = [0u8; 16];
            os_random(&mut salt)?;
            Ok(SqlValue::Text(format!(
                "$2a${cost:02}${}",
                bcrypt_b64_encode(&salt)
            )))
        }
        "des" | "xdes" | "md5" => Err(SqlError::FeatureNotSupported(
            "gen_salt: only 'bf' salts are supported by GuardianDB".into(),
        )),
        other => Err(SqlError::InvalidParameter(format!(
            "gen_salt: Unknown salt algorithm \"{other}\""
        ))),
    }
}

fn crypt(args: &[SqlValue]) -> Result<SqlValue> {
    let password = arg_text(args, 0, "crypt")?;
    let salt = arg_text(args, 1, "crypt")?;
    if !salt.starts_with("$2") {
        return Err(SqlError::FeatureNotSupported(
            "crypt: only 'bf' salts are supported by GuardianDB".into(),
        ));
    }
    let (version, cost, salt_bytes) = parse_bcrypt_salt(&salt)
        .ok_or_else(|| SqlError::InvalidParameter("crypt: invalid salt".into()))?;
    // `hash_with_salt` truncates passwords beyond bcrypt's 72-byte limit,
    // exactly like PostgreSQL's crypt().
    let parts = bcrypt::hash_with_salt(password.as_bytes(), cost, salt_bytes)
        .map_err(|e| SqlError::Internal(format!("bcrypt: {e}")))?;
    Ok(SqlValue::Text(parts.format_for_version(version)))
}

/// Parse a bcrypt salt (or full stored hash — anything after the first 22
/// salt characters is ignored, which is what makes
/// `crypt(pw, stored) = stored` work as a password check).
fn parse_bcrypt_salt(salt: &str) -> Option<(bcrypt::Version, u32, [u8; 16])> {
    let rest = salt.strip_prefix("$2")?;
    let (version, rest) = match rest.as_bytes().first()? {
        b'a' => (bcrypt::Version::TwoA, &rest[1..]),
        b'b' => (bcrypt::Version::TwoB, &rest[1..]),
        b'y' => (bcrypt::Version::TwoY, &rest[1..]),
        _ => return None,
    };
    let rest = rest.strip_prefix('$')?;
    let cost_str = rest.get(..2)?;
    if !cost_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let cost: u32 = cost_str.parse().ok()?;
    if !(4..=31).contains(&cost) {
        return None;
    }
    let rest = rest.get(2..)?.strip_prefix('$')?;
    let salt_bytes = bcrypt_b64_decode(rest.get(..22)?)?;
    Some((version, cost, salt_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn run(name: &str, args: &[SqlValue]) -> Result<SqlValue> {
        let vars = RefCell::new(HashMap::new());
        let ctx = ExtCtx {
            now: Utc::now(),
            vars: &vars,
        };
        call(&ctx, name, args)
    }

    fn text(s: &str) -> SqlValue {
        SqlValue::Text(s.to_string())
    }

    fn bytea(b: &[u8]) -> SqlValue {
        SqlValue::Bytea(b.to_vec())
    }

    fn bytes_of(v: SqlValue) -> Vec<u8> {
        match v {
            SqlValue::Bytea(b) => b,
            other => panic!("expected bytea, got {other:?}"),
        }
    }

    fn text_of(v: SqlValue) -> String {
        match v {
            SqlValue::Text(s) => s,
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn digest_hex(data: &str, algorithm: &str) -> String {
        hex::encode(bytes_of(
            run("digest", &[text(data), text(algorithm)]).unwrap(),
        ))
    }

    #[test]
    fn digest_known_answers() {
        assert_eq!(
            digest_hex("abc", "sha256"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest_hex("", "sha256"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(digest_hex("", "md5"), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(digest_hex("abc", "md5"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            digest_hex("", "sha1"),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            digest_hex("abc", "sha1"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            digest_hex("abc", "sha224"),
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
        assert_eq!(
            digest_hex("abc", "sha384"),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
             8086072ba1e7cc2358baeca134c825a7"
        );
        assert_eq!(
            digest_hex("abc", "sha512"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn digest_accepts_bytea_and_uppercase_algorithm() {
        let out = run("digest", &[bytea(b"abc"), text("SHA256")]).unwrap();
        assert_eq!(
            hex::encode(bytes_of(out)),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn digest_unknown_algorithm() {
        let err = run("digest", &[text("abc"), text("sha3")]).unwrap_err();
        assert_eq!(
            err,
            SqlError::InvalidParameter("Cannot use \"sha3\": No such hash algorithm".into())
        );
    }

    #[test]
    fn hmac_rfc_known_answers() {
        // RFC 4231 test case 2 (HMAC-SHA-256).
        let data = "what do ya want for nothing?";
        let out = run("hmac", &[text(data), text("Jefe"), text("sha256")]).unwrap();
        assert_eq!(
            hex::encode(bytes_of(out)),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 2202 test case 2 (HMAC-MD5 and HMAC-SHA-1, same key/data).
        let out = run("hmac", &[text(data), text("Jefe"), text("md5")]).unwrap();
        assert_eq!(
            hex::encode(bytes_of(out)),
            "750c783e6ab0b503eaa86e310a5db738"
        );
        let out = run("hmac", &[text(data), text("Jefe"), text("sha1")]).unwrap();
        assert_eq!(
            hex::encode(bytes_of(out)),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
    }

    #[test]
    fn hmac_unknown_algorithm() {
        let err = run("hmac", &[text("data"), text("key"), text("crc32")]).unwrap_err();
        assert_eq!(
            err,
            SqlError::InvalidParameter("Cannot use \"crc32\": No such hash algorithm".into())
        );
    }

    #[test]
    fn encode_decode_hex() {
        let enc = text_of(run("encode", &[bytea(b"\x00\xffAZ"), text("hex")]).unwrap());
        assert_eq!(enc, "00ff415a");
        let dec = bytes_of(run("decode", &[text("00ff415a"), text("hex")]).unwrap());
        assert_eq!(dec, b"\x00\xffAZ");
        // Uppercase digits and embedded whitespace are accepted, like PostgreSQL.
        let dec = bytes_of(run("decode", &[text("00 FF\n41\t5A"), text("hex")]).unwrap());
        assert_eq!(dec, b"\x00\xffAZ");
        let err = run("decode", &[text("0g"), text("hex")]).unwrap_err();
        assert_eq!(
            err,
            SqlError::InvalidTextRepresentation {
                ty: "hex data".into(),
                value: "0g".into(),
            }
        );
        // Odd number of digits.
        assert!(run("decode", &[text("abc"), text("hex")]).is_err());
    }

    #[test]
    fn encode_decode_base64() {
        let enc = text_of(run("encode", &[bytea(b"any carnal pleasure"), text("base64")]).unwrap());
        assert_eq!(enc, "YW55IGNhcm5hbCBwbGVhc3VyZQ==");
        // Embedded newlines (PostgreSQL wraps base64 output) are tolerated.
        let dec = bytes_of(
            run(
                "decode",
                &[text("YW55IGNhcm5hbCBw\nbGVhc3VyZQ=="), text("base64")],
            )
            .unwrap(),
        );
        assert_eq!(dec, b"any carnal pleasure");
        let err = run("decode", &[text("!!!"), text("base64")]).unwrap_err();
        assert_eq!(
            err,
            SqlError::InvalidTextRepresentation {
                ty: "base64 data".into(),
                value: "!!!".into(),
            }
        );
    }

    #[test]
    fn encode_decode_escape() {
        // NUL becomes octal, backslash doubles, printable ASCII passes through.
        let enc = text_of(run("encode", &[bytea(b"\x00abc\\\x07\xff"), text("escape")]).unwrap());
        assert_eq!(enc, "\\000abc\\\\\\007\\377");
        let dec =
            bytes_of(run("decode", &[text("\\000abc\\\\\\007\\377"), text("escape")]).unwrap());
        assert_eq!(dec, b"\x00abc\\\x07\xff");
        // Plain text is unchanged in both directions.
        assert_eq!(
            text_of(run("encode", &[bytea(b"hello"), text("escape")]).unwrap()),
            "hello"
        );
        assert_eq!(
            bytes_of(run("decode", &[text("hello"), text("escape")]).unwrap()),
            b"hello"
        );
        // A lone backslash and an out-of-range octal escape are invalid.
        assert!(run("decode", &[text("\\"), text("escape")]).is_err());
        assert!(run("decode", &[text("\\400"), text("escape")]).is_err());
    }

    #[test]
    fn unknown_encoding_format() {
        assert_eq!(
            run("encode", &[bytea(b"x"), text("rot13")]).unwrap_err(),
            SqlError::InvalidParameter("unrecognized encoding: \"rot13\"".into())
        );
        assert!(matches!(
            run("decode", &[text("x"), text("rot13")]).unwrap_err(),
            SqlError::InvalidParameter(_)
        ));
    }

    #[test]
    fn gen_random_bytes_lengths_and_range() {
        for n in [1, 16, 1024] {
            let out = bytes_of(run("gen_random_bytes", &[SqlValue::Int4(n)]).unwrap());
            assert_eq!(out.len(), n as usize);
        }
        let a = bytes_of(run("gen_random_bytes", &[SqlValue::Int4(16)]).unwrap());
        let b = bytes_of(run("gen_random_bytes", &[SqlValue::Int4(16)]).unwrap());
        assert_ne!(a, b, "two 128-bit draws must differ");
        for n in [0, -1, 1025] {
            assert_eq!(
                run("gen_random_bytes", &[SqlValue::Int4(n)]).unwrap_err(),
                SqlError::InvalidParameter("Length not in range 1..1024".into())
            );
        }
    }

    #[test]
    fn gen_random_uuid_is_v4_and_unique() {
        let uuid_of = |v: SqlValue| match v {
            SqlValue::Uuid(u) => u,
            other => panic!("expected uuid, got {other:?}"),
        };
        let a = uuid_of(run("gen_random_uuid", &[]).unwrap());
        let b = uuid_of(run("gen_random_uuid", &[]).unwrap());
        assert_eq!(a.get_version_num(), 4);
        assert_ne!(a, b);
    }

    #[test]
    fn gen_salt_shape() {
        let salt = text_of(run("gen_salt", &[text("bf")]).unwrap());
        assert_eq!(salt.len(), 29);
        assert!(salt.starts_with("$2a$06$"), "default cost is 6: {salt}");
        assert!(
            salt[7..].bytes().all(|b| BCRYPT_ALPHABET.contains(&b)),
            "salt characters must come from the bcrypt alphabet: {salt}"
        );
        let salt = text_of(run("gen_salt", &[text("bf"), SqlValue::Int4(10)]).unwrap());
        assert!(salt.starts_with("$2a$10$"));
        // gen_salt output feeds crypt() directly.
        assert!(parse_bcrypt_salt(&salt).is_some());
    }

    #[test]
    fn gen_salt_errors() {
        for n in [3, 32, 0, -1] {
            assert_eq!(
                run("gen_salt", &[text("bf"), SqlValue::Int4(n)]).unwrap_err(),
                SqlError::InvalidParameter("gen_salt: Incorrect number of rounds".into())
            );
        }
        for ty in ["md5", "xdes", "des"] {
            assert_eq!(
                run("gen_salt", &[text(ty)]).unwrap_err(),
                SqlError::FeatureNotSupported(
                    "gen_salt: only 'bf' salts are supported by GuardianDB".into()
                )
            );
        }
        assert!(matches!(
            run("gen_salt", &[text("scrypt")]).unwrap_err(),
            SqlError::InvalidParameter(_)
        ));
    }

    #[test]
    fn crypt_known_answer() {
        // crypt_blowfish's canonical test vector.
        let out = text_of(
            run(
                "crypt",
                &[text("U*U"), text("$2a$05$CCCCCCCCCCCCCCCCCCCCC.")],
            )
            .unwrap(),
        );
        assert_eq!(
            out,
            "$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW"
        );
        // Re-crypting with the stored hash as the salt reproduces it — the
        // PostgreSQL password-check idiom.
        let again = text_of(run("crypt", &[text("U*U"), text(&out)]).unwrap());
        assert_eq!(again, out);
    }

    #[test]
    fn crypt_round_trip_and_mismatch() {
        let salt = text_of(run("gen_salt", &[text("bf"), SqlValue::Int4(4)]).unwrap());
        let stored = text_of(run("crypt", &[text("correct horse"), text(&salt)]).unwrap());
        assert_eq!(stored.len(), 60);
        assert!(stored.starts_with(&salt), "the hash embeds its salt");
        let check = text_of(run("crypt", &[text("correct horse"), text(&stored)]).unwrap());
        assert_eq!(check, stored);
        let wrong = text_of(run("crypt", &[text("wrong horse"), text(&stored)]).unwrap());
        assert_ne!(wrong, stored);
    }

    #[test]
    fn crypt_preserves_salt_version() {
        let out = text_of(
            run(
                "crypt",
                &[text("U*U"), text("$2b$05$CCCCCCCCCCCCCCCCCCCCC.")],
            )
            .unwrap(),
        );
        assert!(out.starts_with("$2b$05$"), "version prefix survives: {out}");
        let again = text_of(run("crypt", &[text("U*U"), text(&out)]).unwrap());
        assert_eq!(again, out);
    }

    #[test]
    fn crypt_rejects_bad_salts() {
        let unsupported = SqlError::FeatureNotSupported(
            "crypt: only 'bf' salts are supported by GuardianDB".into(),
        );
        assert_eq!(
            run("crypt", &[text("pw"), text("ab")]).unwrap_err(),
            unsupported
        );
        assert_eq!(
            run("crypt", &[text("pw"), text("$1$abcdefgh")]).unwrap_err(),
            unsupported
        );
        let invalid = SqlError::InvalidParameter("crypt: invalid salt".into());
        // Cost out of range, truncated salt, character outside the alphabet.
        assert_eq!(
            run(
                "crypt",
                &[text("pw"), text("$2a$99$CCCCCCCCCCCCCCCCCCCCC.")]
            )
            .unwrap_err(),
            invalid
        );
        assert_eq!(
            run("crypt", &[text("pw"), text("$2a$05$short")]).unwrap_err(),
            invalid
        );
        assert_eq!(
            run(
                "crypt",
                &[text("pw"), text("$2a$05$!!!!!!!!!!!!!!!!!!!!!!")]
            )
            .unwrap_err(),
            invalid
        );
    }

    #[test]
    fn null_arguments_yield_null() {
        let null = SqlValue::Null;
        let cases: Vec<(&str, Vec<SqlValue>)> = vec![
            ("digest", vec![null.clone(), text("sha256")]),
            ("digest", vec![text("abc"), null.clone()]),
            ("hmac", vec![text("a"), null.clone(), text("sha256")]),
            ("gen_random_bytes", vec![null.clone()]),
            ("encode", vec![null.clone(), text("hex")]),
            ("decode", vec![text("00"), null.clone()]),
            ("gen_salt", vec![null.clone()]),
            ("gen_salt", vec![text("bf"), null.clone()]),
            ("crypt", vec![text("pw"), null.clone()]),
            ("crypt", vec![null, text("$2a$06$xxxxxxxxxxxxxxxxxxxxxx")]),
        ];
        for (name, args) in cases {
            assert!(
                matches!(run(name, &args).unwrap(), SqlValue::Null),
                "{name} must be strict"
            );
        }
    }

    #[test]
    fn unrouted_name_is_internal_error() {
        assert!(matches!(
            run("pgp_sym_encrypt", &[]).unwrap_err(),
            SqlError::Internal(_)
        ));
    }

    #[test]
    fn bcrypt_base64_round_trip() {
        let bytes: [u8; 16] = *b"0123456789abcdef";
        let encoded = bcrypt_b64_encode(&bytes);
        assert_eq!(encoded.len(), 22);
        assert_eq!(bcrypt_b64_decode(&encoded), Some(bytes));
    }
}
