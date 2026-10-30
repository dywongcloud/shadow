//! The `hive/browser/0` wire contract shared by the browser tab (`hive-browser`) and fleet nodes
//! (`hive-p2p`). Request: `[u32 le total_len][op][payload]`; reply: `[u32 le len][bytes]` with no
//! op byte. Lengths are little-endian here and big-endian on `HIVE_ALPN` — separate protocols,
//! never unify them.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// ALPN for browser-tab peers — a low-trust surface disjoint from `hive_p2p::HIVE_ALPN`'s
/// mode-byte dispatch.
pub const BROWSER_ALPN: &[u8] = b"hive/browser/0";

/// The single admission/control version THIS build's browser side speaks.
pub const BROWSER_PROTOCOL_VERSION: u16 = 0;

/// Client `protocol_version` range THIS server build admits; widen `..MAX` on the fleet before
/// shipping a client bump.
pub const BROWSER_PROTOCOL_MIN: u16 = 0;
pub const BROWSER_PROTOCOL_MAX: u16 = 0;

/// Where a client version falls relative to this server: outdated-reload and
/// server-not-rolled-yet are distinct outcomes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProtocolFit {
    /// Below [`BROWSER_PROTOCOL_MIN`] — the client bundle is durably outdated;
    /// no retry without a reload will ever succeed against this server.
    TooOld,
    /// Within `[BROWSER_PROTOCOL_MIN, BROWSER_PROTOCOL_MAX]` — accepted.
    Supported,
    /// Above [`BROWSER_PROTOCOL_MAX`] — normal mid-rollout (other nodes may accept it); bounded
    /// retry, never a forced reload.
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

/// Cap on a single request frame, checked by both sides before allocating.
pub const BROWSER_MAX_FRAME: usize = 1024 * 1024;

/// Echo is diagnostic, not bulk transfer; its payload cap stays far below function/asset frames.
pub const BROWSER_MAX_ECHO: usize = 64 * 1024;

/// Frame cap for [`Op::CrrSync`]: one CRR value may be up to the deployment's `max_value_bytes`
/// (default 1 MiB) plus a batch and watermarks, checked BEFORE allocation; a value cap raised
/// past this gets a typed refusal, never truncation.
pub const BROWSER_MAX_CRR_FRAME: usize = 4 * 1024 * 1024;

/// Stream reset codes shared by both sides.
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
    /// The endpoint is not granted the requested function digest — policy, distinct from
    /// [`NO_HANDLER`].
    pub const FORBIDDEN: u32 = 7;
    /// This browser node has no bounded connection or stream slot available.
    /// Callers may retry after another operation releases its Drop-owned permit.
    pub const OVERLOADED: u32 = 8;
    /// The peer did not finish a handshake/frame or a local async handler did
    /// not settle within the browser node's explicit deadline.
    pub const DEADLINE_EXCEEDED: u32 = 9;
}

/// Functions and assets use iroh's lowercase BLAKE3 text form; one fixed width keeps a SHA-256
/// hex from aliasing a BLAKE3 object.
pub const BLAKE3_DIGEST_LEN: usize = 64;
pub const FUNCTION_DIGEST_LEN: usize = BLAKE3_DIGEST_LEN;
pub const ASSET_DIGEST_LEN: usize = BLAKE3_DIGEST_LEN;

/// Asset replies reserve eight bytes for the complete object length; the rest is one verified
/// chunk.
pub const ASSET_REPLY_META_LEN: usize = 8;
pub const ASSET_CHUNK_MAX: usize = BROWSER_MAX_FRAME - ASSET_REPLY_META_LEN;

/// Maximum complete asset the whole-object browser API (`assetOn`, one `Uint8Array`) returns.
pub const BROWSER_MAX_ASSET: usize = 64 * 1024 * 1024;

/// The op selector, first byte of every request. An unknown byte is refused with
/// [`reset::UNKNOWN_OP`], never defaulted to [`Op::Echo`].
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
    /// One browser↔fleet CRR anti-entropy round (docs/browser-db-contract.md): sync-domain refusals
    /// are reply statuses; protocol faults stay stream resets.
    CrrSync = 3,
}

impl Op {
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    /// The frame cap for requests carrying this op, checked after the op byte and BEFORE the payload
    /// is allocated.
    pub const fn frame_cap(self) -> usize {
        match self {
            Op::Echo | Op::Invoke | Op::AssetGet => BROWSER_MAX_FRAME,
            Op::CrrSync => BROWSER_MAX_CRR_FRAME,
        }
    }

    /// Parse an op byte. `Err` carries the unrecognised byte so the refusal can
    /// name it in a log line.
    pub const fn from_byte(b: u8) -> Result<Self, ProtoError> {
        match b {
            0 => Ok(Op::Echo),
            1 => Ok(Op::Invoke),
            2 => Ok(Op::AssetGet),
            3 => Ok(Op::CrrSync),
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
    /// A `CrrSync` request or reply payload was truncated, non-canonical, over
    /// a bound, or carried trailing bytes.
    MalformedCrrSync,
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
            | ProtoError::MalformedAsset
            | ProtoError::MalformedCrrSync => reset::MALFORMED_PAYLOAD,
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
            ProtoError::MalformedCrrSync => write!(f, "malformed crr sync payload"),
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

/// Per-op [`check_len`]: a `CrrSync` frame may exceed [`BROWSER_MAX_FRAME`] while an `Echo` of
/// that size is refused.
pub const fn check_len_for(op: Op, len_le: [u8; 4]) -> Result<usize, ProtoError> {
    let len = u32::from_le_bytes(len_le) as usize;
    if len > op.frame_cap() {
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

/// Build an [`Op::Invoke`] payload `[64-byte code_digest][request_json]`; the callee resolves
/// the digest to a pinned artifact.
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

/// `CrrSync` payload version. Bump on any shape change; peers refuse a version
/// they do not know rather than misparse it.
pub const CRR_SYNC_VERSION: u8 = 1;

/// Largest site-id blob on the wire (crsql site ids are 16 bytes; HCB1 permits 255).
pub const CRR_SITE_ID_MAX: usize = 64;
/// Bound on the watermark advertisement (one entry per known origin site).
pub const CRR_MAX_WATERMARKS: usize = 1024;
/// Bound on batches per frame. Wire size is the real limit (the frame cap);
/// this exists so a corrupt count cannot drive a huge pre-allocation.
pub const CRR_MAX_BATCHES: usize = 4096;
/// Bound on the reply's diagnostic message.
pub const CRR_MAX_MESSAGE: usize = 2048;
/// Bound on the request's `db_file` grant identifier (a platform-templated
/// replica name, never a path).
pub const CRR_MAX_DB_FILE: usize = 256;

/// Request flag: the sender has MORE push batches than fit this frame; the
/// responder should expect follow-up rounds before the push stream is done.
pub const CRR_FLAG_PUSH_MORE: u8 = 1;
/// Reply flag: the responder's export was truncated to fit the frame; the
/// requester re-requests (its applied watermarks are the resume cursor).
pub const CRR_FLAG_MORE: u8 = 1;

/// The APPLY half's typed outcome; sync-domain refusals travel in the reply, protocol faults
/// stay stream resets.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CrrStatus {
    /// Every presented batch applied (or replayed as a no-op).
    Ok = 0,
    /// A presented batch chained from ahead of the responder's durable watermark (hive-crsql
    /// `SyncGap`); nothing written, re-export from the reply's watermark.
    SyncGap = 1,
    /// Applying a presented batch would push the replica past the spec's
    /// `max_bytes` — the batch rolled back whole, never truncated.
    QuotaExceeded = 2,
    /// A presented value exceeds the spec's `max_value_bytes`; the whole batch was refused and the
    /// message names the first offender.
    ValueTooLarge = 3,
    /// The grant behind this request is read-only (Public-scope admission):
    /// no presented batch was applied. The export half is still served.
    ReadOnly = 4,
    /// A presented HCB1 batch decoded but is unusable on this wire (empty site id, out-of-order
    /// payload); refused whole.
    BatchRefused = 5,
}

impl CrrStatus {
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    pub const fn from_byte(b: u8) -> Result<Self, ProtoError> {
        match b {
            0 => Ok(CrrStatus::Ok),
            1 => Ok(CrrStatus::SyncGap),
            2 => Ok(CrrStatus::QuotaExceeded),
            3 => Ok(CrrStatus::ValueTooLarge),
            4 => Ok(CrrStatus::ReadOnly),
            5 => Ok(CrrStatus::BatchRefused),
            _ => Err(ProtoError::MalformedCrrSync),
        }
    }
}

/// Decoded `Op::CrrSync` request body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrrSyncRequest {
    /// The platform-templated replica name from the sender's admission capability; the responder
    /// NEVER opens it, only compares it with its own server-derived name.
    pub db_file: String,
    /// `CRR_FLAG_PUSH_MORE` while the sender's push stream continues.
    pub push_more: bool,
    /// The sender's durable per-site watermarks: `(site_id, db_version)`.
    pub watermarks: Vec<(Vec<u8>, i64)>,
    /// Outbound HCB1 batches (verbatim `hive-crsql` `ChangeBatch::encode`
    /// frames), applied in order by the responder.
    pub batches: Vec<Vec<u8>>,
}

/// Decoded `Op::CrrSync` reply body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrrSyncReply {
    /// The apply half's typed outcome (see [`CrrStatus`]).
    pub status: CrrStatus,
    /// The responder has more export batches than fit this frame.
    pub more: bool,
    /// Human/operator diagnostic for non-Ok statuses (site hex, table/pk,
    /// watermark numbers); empty on Ok. Bounded by [`CRR_MAX_MESSAGE`].
    pub message: String,
    /// The responder's durable per-site watermarks AFTER any apply — the
    /// acknowledgement the requester persists as its push cursor.
    pub watermarks: Vec<(Vec<u8>, i64)>,
    /// The responder's export batches for the requester, in apply order.
    pub batches: Vec<Vec<u8>>,
}

fn push_watermarks(out: &mut Vec<u8>, watermarks: &[(Vec<u8>, i64)]) {
    out.extend_from_slice(&(watermarks.len() as u32).to_be_bytes());
    for (site, version) in watermarks {
        out.push(site.len() as u8);
        out.extend_from_slice(site);
        out.extend_from_slice(&version.to_be_bytes());
    }
}

fn push_batches(out: &mut Vec<u8>, batches: &[Vec<u8>]) {
    out.extend_from_slice(&(batches.len() as u32).to_be_bytes());
    for batch in batches {
        out.extend_from_slice(&(batch.len() as u32).to_be_bytes());
        out.extend_from_slice(batch);
    }
}

/// Cursor over a payload slice: exact-EOF consume, every take bounds-checked,
/// trailing bytes rejected by the callers.
struct CrrCursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> CrrCursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.b.len() - self.at < n {
            return Err(ProtoError::MalformedCrrSync);
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_be_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| ProtoError::MalformedCrrSync)?,
        ))
    }

    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| ProtoError::MalformedCrrSync)?,
        ))
    }

    fn i64(&mut self) -> Result<i64, ProtoError> {
        Ok(i64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| ProtoError::MalformedCrrSync)?,
        ))
    }

    fn watermarks(&mut self) -> Result<Vec<(Vec<u8>, i64)>, ProtoError> {
        let count = self.u32()? as usize;
        if count > CRR_MAX_WATERMARKS {
            return Err(ProtoError::MalformedCrrSync);
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let site_len = self.u8()? as usize;
            if site_len == 0 || site_len > CRR_SITE_ID_MAX {
                return Err(ProtoError::MalformedCrrSync);
            }
            let site = self.take(site_len)?.to_vec();
            let version = self.i64()?;
            out.push((site, version));
        }
        Ok(out)
    }

    fn batches(&mut self) -> Result<Vec<Vec<u8>>, ProtoError> {
        let count = self.u32()? as usize;
        if count > CRR_MAX_BATCHES {
            return Err(ProtoError::MalformedCrrSync);
        }
        let mut out = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            let len = self.u32()? as usize;
            if len > BROWSER_MAX_CRR_FRAME {
                return Err(ProtoError::MalformedCrrSync);
            }
            out.push(self.take(len)?.to_vec());
        }
        Ok(out)
    }

    fn finish(self) -> Result<(), ProtoError> {
        if self.at != self.b.len() {
            return Err(ProtoError::MalformedCrrSync);
        }
        Ok(())
    }
}

/// Build an [`Op::CrrSync`] request payload.
pub fn encode_crr_sync_request(request: &CrrSyncRequest) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + request.batches.iter().map(Vec::len).sum::<usize>());
    out.push(CRR_SYNC_VERSION);
    out.push(if request.push_more {
        CRR_FLAG_PUSH_MORE
    } else {
        0
    });
    let db_file = request.db_file.as_bytes();
    let db_file = &db_file[..db_file.len().min(CRR_MAX_DB_FILE)];
    out.extend_from_slice(&(db_file.len() as u16).to_be_bytes());
    out.extend_from_slice(db_file);
    push_watermarks(&mut out, &request.watermarks);
    push_batches(&mut out, &request.batches);
    out
}

/// Inverse of [`encode_crr_sync_request`]. A hostile peer controls every byte
/// here: every length is bounded before allocation and trailing bytes refuse.
pub fn split_crr_sync_request(payload: &[u8]) -> Result<CrrSyncRequest, ProtoError> {
    let mut cur = CrrCursor { b: payload, at: 0 };
    if cur.u8()? != CRR_SYNC_VERSION {
        return Err(ProtoError::MalformedCrrSync);
    }
    let flags = cur.u8()?;
    let db_len = cur.u16()? as usize;
    if db_len == 0 || db_len > CRR_MAX_DB_FILE {
        return Err(ProtoError::MalformedCrrSync);
    }
    let db_file =
        String::from_utf8(cur.take(db_len)?.to_vec()).map_err(|_| ProtoError::MalformedCrrSync)?;
    let watermarks = cur.watermarks()?;
    let batches = cur.batches()?;
    cur.finish()?;
    Ok(CrrSyncRequest {
        db_file,
        push_more: flags & CRR_FLAG_PUSH_MORE != 0,
        watermarks,
        batches,
    })
}

/// Build an [`Op::CrrSync`] reply payload.
pub fn encode_crr_sync_reply(reply: &CrrSyncReply) -> Vec<u8> {
    let message = reply.message.as_bytes();
    let message = &message[..message.len().min(CRR_MAX_MESSAGE)];
    let mut out = Vec::with_capacity(64 + reply.batches.iter().map(Vec::len).sum::<usize>());
    out.push(CRR_SYNC_VERSION);
    out.push(reply.status.as_byte());
    out.push(if reply.more { CRR_FLAG_MORE } else { 0 });
    out.extend_from_slice(&(message.len() as u16).to_be_bytes());
    out.extend_from_slice(message);
    push_watermarks(&mut out, &reply.watermarks);
    push_batches(&mut out, &reply.batches);
    out
}

/// Inverse of [`encode_crr_sync_reply`]; same hostile-input discipline as
/// [`split_crr_sync_request`].
pub fn split_crr_sync_reply(payload: &[u8]) -> Result<CrrSyncReply, ProtoError> {
    let mut cur = CrrCursor { b: payload, at: 0 };
    if cur.u8()? != CRR_SYNC_VERSION {
        return Err(ProtoError::MalformedCrrSync);
    }
    let status = CrrStatus::from_byte(cur.u8()?)?;
    let flags = cur.u8()?;
    let msg_len = cur.u16()? as usize;
    if msg_len > CRR_MAX_MESSAGE {
        return Err(ProtoError::MalformedCrrSync);
    }
    let message =
        String::from_utf8(cur.take(msg_len)?.to_vec()).map_err(|_| ProtoError::MalformedCrrSync)?;
    let watermarks = cur.watermarks()?;
    let batches = cur.batches()?;
    cur.finish()?;
    Ok(CrrSyncReply {
        status,
        more: flags & CRR_FLAG_MORE != 0,
        message,
        watermarks,
        batches,
    })
}
