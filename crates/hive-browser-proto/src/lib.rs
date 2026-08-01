//! The `hive/browser/0` wire contract — one definition, two implementations.
//!
//! A browser tab ([`hive-browser`], wasm32) and a fleet node ([`hive-p2p`],
//! native) both speak this protocol, and they must agree byte-for-byte. They
//! used to agree by having the same constants typed out twice with a comment
//! asking future editors to keep them in sync; the first byte added to the
//! protocol (the op selector below) immediately broke that, because only one of
//! the two sides learned about it. Everything the two sides must agree on lives
//! here instead, so drift is a compile error rather than a silent wire bug.
//!
//! # Framing
//!
//! A request is `[u32 le total_len][op][op_payload]`, where `total_len` counts
//! the op byte. A reply is `[u32 le len][bytes]` with **no op byte** — the
//! caller already knows what it asked for, and echoing the op back is precisely
//! the bug that motivated this crate (a verbatim echo returns the op byte as
//! part of the reply text).
//!
//! Lengths are little-endian here and big-endian on `HIVE_ALPN`'s own
//! `read_frame`/`write_frame`. That is not an oversight to "fix": these are
//! separate protocols on separate ALPNs, and quietly changing either one's
//! endianness breaks every peer already speaking it.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// ALPN for browser-tab peers — a dedicated, low-trust surface, structurally
/// disjoint from `hive_p2p::HIVE_ALPN`'s gossip/join/raw modes. Connections on
/// this ALPN never reach that mode-byte dispatch.
pub const BROWSER_ALPN: &[u8] = b"hive/browser/0";

/// Admission/control version paired with this ALPN generation. Keep it shared
/// so the PWA and fleet reject skew explicitly instead of duplicating a number.
/// This is the version THIS build's browser side speaks when making an
/// admission request — always a single concrete number, never a range (a
/// build can only ever emit one version of itself).
pub const BROWSER_PROTOCOL_VERSION: u16 = 0;

/// The range of client-declared `protocol_version` values THIS build's
/// server side will admit (bn-p2p-version-negotiation). Both bounds equal
/// [`BROWSER_PROTOCOL_VERSION`] today (no rolling-upgrade window open yet) —
/// widen `..MAX` first on the server fleet during a real protocol bump, THEN
/// ship the client change, so mid-rollout old clients keep working against
/// new servers (server-ahead is normal; client-ahead of the whole fleet is
/// the failure this range exists to reject explicitly rather than silently).
pub const BROWSER_PROTOCOL_MIN: u16 = 0;
pub const BROWSER_PROTOCOL_MAX: u16 = 0;

/// Where a client-declared protocol version falls relative to what this
/// server build accepts — three-way rather than boolean so a caller can
/// surface "you are outdated, reload" (durable, needs a client fix) distinctly
/// from "this server hasn't rolled forward yet" (transient, retry as-is).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProtocolFit {
    /// Below [`BROWSER_PROTOCOL_MIN`] — the client bundle is durably outdated;
    /// no retry without a reload will ever succeed against this server.
    TooOld,
    /// Within `[BROWSER_PROTOCOL_MIN, BROWSER_PROTOCOL_MAX]` — accepted.
    Supported,
    /// Above [`BROWSER_PROTOCOL_MAX`] — the client is ahead of this specific
    /// server node, which is normal mid-rollout (other nodes may already
    /// accept it); transient, worth a bounded retry, never a forced reload.
    TooNew,
}

pub const fn protocol_fit(client_version: u16) -> ProtocolFit {
    if client_version < BROWSER_PROTOCOL_MIN {
        ProtocolFit::TooOld
    } else if client_version > BROWSER_PROTOCOL_MAX {
        ProtocolFit::TooNew
    } else {
        ProtocolFit::Supported
    }
}

/// Cap on a single request frame — the memory-safety line for any peer serving
/// untrusted browser traffic. An unbounded read here is a DoS lever, so both
/// sides check against this before allocating.
pub const BROWSER_MAX_FRAME: usize = 1 << 20; // 1 MiB

/// Stream reset codes. Shared so a reset means the same thing on both sides
/// instead of each end inventing its own numbering.
pub mod reset {
    /// Peer opened a connection with an ALPN this endpoint does not serve.
    pub const UNEXPECTED_ALPN: u32 = 1;
    /// Declared frame length exceeded [`super::BROWSER_MAX_FRAME`].
    pub const FRAME_TOO_LARGE: u32 = 2;
    /// Op byte is not one this build understands.
    pub const UNKNOWN_OP: u32 = 3;
    /// Op is understood but this peer has nothing registered to serve it (e.g.
    /// `Invoke` against a node with no function runtime).
    pub const NO_HANDLER: u32 = 4;
    /// Op is understood and served, but the payload did not parse.
    pub const MALFORMED_PAYLOAD: u32 = 5;
    /// Handler ran and failed. Distinct from [`MALFORMED_PAYLOAD`] so a caller
    /// can tell "I sent something bad" from "your side broke".
    pub const HANDLER_FAILED: u32 = 6;
    /// The authenticated endpoint is not granted the requested function digest.
    /// Kept distinct from [`NO_HANDLER`] so callers cannot confuse policy with a
    /// deployment/runtime fault.
    pub const FORBIDDEN: u32 = 7;
}

/// Functions and assets share iroh's canonical lowercase BLAKE3 text form.
/// Keeping one fixed-width identifier prevents a 64-hex SHA-256 value from
/// becoming an ambiguous alias for a BLAKE3-addressed object.
pub const BLAKE3_DIGEST_LEN: usize = 64;
pub const FUNCTION_DIGEST_LEN: usize = BLAKE3_DIGEST_LEN;
pub const ASSET_DIGEST_LEN: usize = BLAKE3_DIGEST_LEN;

/// Asset replies reserve eight bytes for the complete object length. The rest
/// of the frame is one verified chunk, so arbitrarily large assets never bypass
/// the per-frame allocation cap.
pub const ASSET_REPLY_META_LEN: usize = 8;
pub const ASSET_CHUNK_MAX: usize = BROWSER_MAX_FRAME - ASSET_REPLY_META_LEN;

/// The op selector: the first byte of every request payload.
///
/// Deliberately NOT `#[non_exhaustive]`-with-a-catch-all and NOT defaulting an
/// unrecognised byte to [`Op::Echo`]. An unknown op is a peer speaking a
/// protocol version this build does not have, and the honest response is a loud
/// refusal ([`reset::UNKNOWN_OP`]) rather than silently treating a future
/// `Invoke`-shaped request as an echo and handing back its raw bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    /// Round-trip the payload unchanged. The liveness/reachability proof.
    Echo = 0,
    /// Run a tenant edge function: payload is [`encode_invoke`]'s shape.
    Invoke = 1,
    /// Pull one range of an exact BLAKE3-addressed asset. This is a scoped,
    /// demand-side cache fill, not a general browser CDN serving primitive.
    AssetGet = 2,
}

impl Op {
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    /// Parse an op byte. `Err` carries the unrecognised byte so the refusal can
    /// name it in a log line.
    pub const fn from_byte(b: u8) -> Result<Self, ProtoError> {
        match b {
            0 => Ok(Op::Echo),
            1 => Ok(Op::Invoke),
            2 => Ok(Op::AssetGet),
            other => Err(ProtoError::UnknownOp(other)),
        }
    }
}

/// Every way a peer's bytes can fail to be a valid request.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProtoError {
    /// Declared length exceeds [`BROWSER_MAX_FRAME`]; carries the claim.
    FrameTooLarge(usize),
    /// Op byte is not in [`Op`].
    UnknownOp(u8),
    /// Frame body was empty, so there was no op byte to read.
    EmptyFrame,
    /// An `Invoke` payload was truncated or self-inconsistent.
    MalformedInvoke,
    /// Function digest was not exactly 64 lowercase hexadecimal bytes.
    InvalidFunctionDigest,
    /// An asset range request was truncated, non-canonical, empty, or larger
    /// than a reply frame can carry.
    MalformedAsset,
}

impl ProtoError {
    /// The reset code to send when refusing a stream over this error, so the
    /// mapping is defined once rather than at each call site.
    pub const fn reset_code(self) -> u32 {
        match self {
            ProtoError::FrameTooLarge(_) => reset::FRAME_TOO_LARGE,
            ProtoError::UnknownOp(_) => reset::UNKNOWN_OP,
            ProtoError::EmptyFrame
            | ProtoError::MalformedInvoke
            | ProtoError::InvalidFunctionDigest
            | ProtoError::MalformedAsset => reset::MALFORMED_PAYLOAD,
        }
    }
}

impl core::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProtoError::FrameTooLarge(n) => {
                write!(
                    f,
                    "frame of {n} bytes exceeds the {BROWSER_MAX_FRAME}-byte cap"
                )
            }
            ProtoError::UnknownOp(b) => write!(f, "unknown op byte {b}"),
            ProtoError::EmptyFrame => write!(f, "frame carried no op byte"),
            ProtoError::MalformedInvoke => write!(f, "malformed invoke payload"),
            ProtoError::InvalidFunctionDigest => write!(
                f,
                "function digest must be {FUNCTION_DIGEST_LEN} lowercase hex bytes"
            ),
            ProtoError::MalformedAsset => write!(f, "malformed asset range payload"),
        }
    }
}

impl core::error::Error for ProtoError {}

/// Validate a decoded `[u32 le]` length header against the frame cap. Both
/// sides call this BEFORE allocating a buffer of that size.
pub const fn check_len(len_le: [u8; 4]) -> Result<usize, ProtoError> {
    let len = u32::from_le_bytes(len_le) as usize;
    if len > BROWSER_MAX_FRAME {
        return Err(ProtoError::FrameTooLarge(len));
    }
    Ok(len)
}

/// Build a full request frame: `[u32 le total_len][op][payload]`.
pub fn encode_request(op: Op, payload: &[u8]) -> Vec<u8> {
    let total = (1 + payload.len()) as u32;
    let mut out = Vec::with_capacity(4 + 1 + payload.len());
    out.extend_from_slice(&total.to_le_bytes());
    out.push(op.as_byte());
    out.extend_from_slice(payload);
    out
}

/// Build a reply frame: `[u32 le len][bytes]`, no op byte.
pub fn encode_reply(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Split an already-length-delimited request body into its op and payload.
pub fn split_request(body: &[u8]) -> Result<(Op, &[u8]), ProtoError> {
    let (&op, payload) = body.split_first().ok_or(ProtoError::EmptyFrame)?;
    Ok((Op::from_byte(op)?, payload))
}

/// Return whether `digest` is the one canonical function identifier accepted on
/// the wire: exactly 64 lowercase hexadecimal bytes (a BLAKE3 digest).
pub fn valid_blake3_digest(digest: &str) -> bool {
    digest.len() == BLAKE3_DIGEST_LEN
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn valid_function_digest(digest: &str) -> bool {
    valid_blake3_digest(digest)
}

/// Build an [`Op::Invoke`] payload: `[64-byte code_digest][request_json]`.
/// Executable source is deliberately absent — the callee resolves this digest
/// to a locally pinned artifact.
pub fn encode_invoke(code_digest: &str, request_json: &str) -> Result<Vec<u8>, ProtoError> {
    if !valid_function_digest(code_digest) {
        return Err(ProtoError::InvalidFunctionDigest);
    }
    let mut out = Vec::with_capacity(FUNCTION_DIGEST_LEN + request_json.len());
    out.extend_from_slice(code_digest.as_bytes());
    out.extend_from_slice(request_json.as_bytes());
    Ok(out)
}

/// Inverse of [`encode_invoke`]. Rejects a truncated/non-canonical digest and
/// non-UTF-8 request JSON. A hostile peer controls every byte here.
pub fn split_invoke(payload: &[u8]) -> Result<(&str, &str), ProtoError> {
    let (digest, json) = payload
        .split_at_checked(FUNCTION_DIGEST_LEN)
        .ok_or(ProtoError::MalformedInvoke)?;
    let digest = core::str::from_utf8(digest).map_err(|_| ProtoError::InvalidFunctionDigest)?;
    if !valid_function_digest(digest) {
        return Err(ProtoError::InvalidFunctionDigest);
    }
    let json = core::str::from_utf8(json).map_err(|_| ProtoError::MalformedInvoke)?;
    Ok((digest, json))
}

/// Build an [`Op::AssetGet`] payload:
/// `[64-byte digest][u64 little-endian offset][u32 little-endian max_len]`.
pub fn encode_asset_get(digest: &str, offset: u64, max_len: usize) -> Result<Vec<u8>, ProtoError> {
    if !valid_blake3_digest(digest) || max_len == 0 || max_len > ASSET_CHUNK_MAX {
        return Err(ProtoError::MalformedAsset);
    }
    let mut out = Vec::with_capacity(ASSET_DIGEST_LEN + 12);
    out.extend_from_slice(digest.as_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&(max_len as u32).to_le_bytes());
    Ok(out)
}

/// Parse an asset range payload without allocating.
pub fn split_asset_get(payload: &[u8]) -> Result<(&str, u64, usize), ProtoError> {
    if payload.len() != ASSET_DIGEST_LEN + 12 {
        return Err(ProtoError::MalformedAsset);
    }
    let digest = core::str::from_utf8(&payload[..ASSET_DIGEST_LEN])
        .map_err(|_| ProtoError::MalformedAsset)?;
    if !valid_blake3_digest(digest) {
        return Err(ProtoError::MalformedAsset);
    }
    let mut offset = [0u8; 8];
    offset.copy_from_slice(&payload[ASSET_DIGEST_LEN..ASSET_DIGEST_LEN + 8]);
    let mut max_len = [0u8; 4];
    max_len.copy_from_slice(&payload[ASSET_DIGEST_LEN + 8..]);
    let max_len = u32::from_le_bytes(max_len) as usize;
    if max_len == 0 || max_len > ASSET_CHUNK_MAX {
        return Err(ProtoError::MalformedAsset);
    }
    Ok((digest, u64::from_le_bytes(offset), max_len))
}

/// Prefix an asset chunk with the complete object length.
pub fn encode_asset_reply(total_len: u64, chunk: &[u8]) -> Result<Vec<u8>, ProtoError> {
    if chunk.len() > ASSET_CHUNK_MAX {
        return Err(ProtoError::MalformedAsset);
    }
    let mut out = Vec::with_capacity(ASSET_REPLY_META_LEN + chunk.len());
    out.extend_from_slice(&total_len.to_le_bytes());
    out.extend_from_slice(chunk);
    Ok(out)
}

/// Parse `[u64 total_len][chunk]` and enforce the frame-sized chunk bound.
pub fn split_asset_reply(payload: &[u8]) -> Result<(u64, &[u8]), ProtoError> {
    if payload.len() < ASSET_REPLY_META_LEN || payload.len() > BROWSER_MAX_FRAME {
        return Err(ProtoError::MalformedAsset);
    }
    let mut total = [0u8; 8];
    total.copy_from_slice(&payload[..ASSET_REPLY_META_LEN]);
    Ok((u64::from_le_bytes(total), &payload[ASSET_REPLY_META_LEN..]))
}
