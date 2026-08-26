use anyhow::{bail, Context};
use sha2::{Digest, Sha256};

use crate::runtime_artifact_transfer_wire::{
    decode_request, encode_request, sha256_hex, BeginRequest, ReplyCode, TransferReply,
    TransferRequest, TransferState, MAX_ERROR_BYTES,
};

const SENTINEL: &[u8] = b"HIVE_RUNTIME_ARTIFACT_TRANSFER_STATE_V1\n";
const CHECKSUM_DOMAIN: &[u8] = b"hive-runtime-artifact-transfer-state-v1\0";
// The embedded Begin frame now carries the canonical manifest (up to
// `wire::MAX_BEGIN_BYTES` ≈ 1.6 MiB), so the record ceiling leaves real
// headroom for the contiguous chunk journal beside it.
const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHUNKS: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkSeal {
    pub offset: u64,
    pub bytes: u32,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferRecord {
    pub begin: BeginRequest,
    pub state: TransferState,
    pub next_offset: u64,
    pub chunks: Vec<ChunkSeal>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub lease_expires_ms: u64,
    pub participant_boot_nonce: String,
    pub semantic_tree_sha256: String,
    pub hidden_deployment_id: String,
    pub readiness_sha256: String,
    pub terminal_error: String,
}

impl TransferRecord {
    pub fn new(
        begin: BeginRequest,
        now_ms: u64,
        lease_expires_ms: u64,
        participant_boot_nonce: String,
    ) -> anyhow::Result<Self> {
        let record = Self {
            begin,
            state: TransferState::Receiving,
            next_offset: 0,
            chunks: Vec::new(),
            created_ms: now_ms,
            updated_ms: now_ms,
            lease_expires_ms,
            participant_boot_nonce,
            semantic_tree_sha256: String::new(),
            hidden_deployment_id: String::new(),
            readiness_sha256: String::new(),
            terminal_error: String::new(),
        };
        record.validate()?;
        Ok(record)
    }

    pub fn reply(&self, code: ReplyCode, message: impl Into<String>) -> TransferReply {
        let mut message = message.into();
        if message.len() > MAX_ERROR_BYTES {
            message.truncate(MAX_ERROR_BYTES);
        }
        TransferReply {
            code,
            transaction_id: self.begin.key.transaction_id.clone(),
            generation_sha256: self.begin.key.generation_sha256.clone(),
            state: self.state,
            next_offset: self.next_offset,
            package_bytes: self.begin.package.package_bytes,
            participant_boot_nonce: self.participant_boot_nonce.clone(),
            semantic_tree_sha256: self.semantic_tree_sha256.clone(),
            hidden_deployment_id: self.hidden_deployment_id.clone(),
            readiness_sha256: self.readiness_sha256.clone(),
            message,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        crate::runtime_artifact_transfer_wire::validate_begin(&self.begin)?;
        if self.next_offset > self.begin.package.package_bytes {
            bail!("transfer durable prefix exceeds declared package size");
        }
        if self.chunks.len() > MAX_CHUNKS {
            bail!("transfer chunk journal exceeds its fixed limit");
        }
        let mut expected = 0u64;
        for chunk in &self.chunks {
            if chunk.offset != expected || chunk.bytes == 0 {
                bail!("transfer chunk journal is not one contiguous non-empty prefix");
            }
            validate_digest(&chunk.sha256, "transfer chunk digest")?;
            expected = expected
                .checked_add(chunk.bytes as u64)
                .context("transfer chunk journal offset overflow")?;
            if expected > self.begin.package.package_bytes {
                bail!("transfer chunk journal exceeds declared package size");
            }
        }
        if expected != self.next_offset {
            bail!("transfer durable prefix differs from its chunk journal");
        }
        validate_text(
            &self.participant_boot_nonce,
            128,
            false,
            "participant boot nonce",
        )?;
        validate_optional_digest(
            &self.semantic_tree_sha256,
            "materialized semantic tree digest",
        )?;
        validate_text(
            &self.hidden_deployment_id,
            256,
            true,
            "hidden deployment id",
        )?;
        validate_optional_digest(&self.readiness_sha256, "readiness digest")?;
        validate_text(
            &self.terminal_error,
            MAX_ERROR_BYTES,
            true,
            "terminal transfer error",
        )?;
        if matches!(
            self.state,
            TransferState::Finalizing
                | TransferState::Materialized
                | TransferState::Prepared
                | TransferState::Committed
        ) && self.next_offset != self.begin.package.package_bytes
        {
            bail!("post-upload transfer state does not hold the complete package");
        }
        if matches!(
            self.state,
            TransferState::Materialized | TransferState::Prepared | TransferState::Committed
        ) && self.semantic_tree_sha256 != self.begin.package.semantic_tree_sha256
        {
            bail!("materialized transfer state lacks its exact semantic identity");
        }
        if matches!(
            self.state,
            TransferState::Prepared | TransferState::Committed
        ) && (self.hidden_deployment_id.is_empty() || self.readiness_sha256.is_empty())
        {
            bail!("prepared transfer state lacks hidden deployment readiness authority");
        }
        if self.state == TransferState::Failed && self.terminal_error.is_empty() {
            bail!("failed transfer state lacks a bounded terminal error");
        }
        Ok(())
    }
}

pub fn encode_record(record: &TransferRecord) -> anyhow::Result<Vec<u8>> {
    record.validate()?;
    let begin = encode_request(&TransferRequest::Begin(record.begin.clone()))?;
    let mut payload = Vec::new();
    put_bytes(&mut payload, &begin)?;
    payload.push(record.state as u8);
    put_u64(&mut payload, record.next_offset);
    put_u64(&mut payload, record.created_ms);
    put_u64(&mut payload, record.updated_ms);
    put_u64(&mut payload, record.lease_expires_ms);
    put_text(&mut payload, &record.participant_boot_nonce, 128, false)?;
    put_text(&mut payload, &record.semantic_tree_sha256, 64, true)?;
    put_text(&mut payload, &record.hidden_deployment_id, 256, true)?;
    put_text(&mut payload, &record.readiness_sha256, 64, true)?;
    put_text(&mut payload, &record.terminal_error, MAX_ERROR_BYTES, true)?;
    let chunk_count = u32::try_from(record.chunks.len()).context("chunk journal count overflow")?;
    put_u32(&mut payload, chunk_count);
    for chunk in &record.chunks {
        put_u64(&mut payload, chunk.offset);
        put_u32(&mut payload, chunk.bytes);
        payload.extend_from_slice(chunk.sha256.as_bytes());
    }
    if payload.len() > MAX_RECORD_BYTES {
        bail!("runtime artifact transfer durable state exceeds its fixed limit");
    }
    let checksum = checksum(&payload);
    let mut output = Vec::with_capacity(SENTINEL.len() + 65 + payload.len());
    output.extend_from_slice(SENTINEL);
    output.extend_from_slice(checksum.as_bytes());
    output.push(b'\n');
    output.extend_from_slice(&payload);
    Ok(output)
}

pub fn decode_record(bytes: &[u8]) -> anyhow::Result<TransferRecord> {
    if bytes.len() > MAX_RECORD_BYTES + SENTINEL.len() + 65 {
        bail!("runtime artifact transfer durable state exceeds its fixed limit");
    }
    let rest = bytes
        .strip_prefix(SENTINEL)
        .context("runtime artifact transfer durable state has invalid sentinel")?;
    if rest.len() < 65 || rest[64] != b'\n' {
        bail!("runtime artifact transfer durable state has invalid checksum header");
    }
    let expected = std::str::from_utf8(&rest[..64])?;
    validate_digest(expected, "transfer state checksum")?;
    let payload = &rest[65..];
    if checksum(payload) != expected {
        bail!("runtime artifact transfer durable state checksum mismatch");
    }
    let mut reader = Reader::new(payload);
    let begin_frame = reader.bytes(MAX_RECORD_BYTES)?;
    let begin = match decode_request(begin_frame)? {
        TransferRequest::Begin(begin) => begin,
        _ => bail!("transfer durable state embeds a non-begin request"),
    };
    let state = TransferState::decode(reader.u8()?)?;
    let next_offset = reader.u64()?;
    let created_ms = reader.u64()?;
    let updated_ms = reader.u64()?;
    let lease_expires_ms = reader.u64()?;
    let participant_boot_nonce = reader.text(128, false)?.to_string();
    let semantic_tree_sha256 = reader.text(64, true)?.to_string();
    let hidden_deployment_id = reader.text(256, true)?.to_string();
    let readiness_sha256 = reader.text(64, true)?.to_string();
    let terminal_error = reader.text(MAX_ERROR_BYTES, true)?.to_string();
    let count = reader.u32()? as usize;
    if count > MAX_CHUNKS {
        bail!("transfer chunk journal exceeds its fixed limit");
    }
    let mut chunks = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let offset = reader.u64()?;
        let bytes = reader.u32()?;
        let sha256 = std::str::from_utf8(reader.take(64)?)?.to_string();
        chunks.push(ChunkSeal {
            offset,
            bytes,
            sha256,
        });
    }
    reader.finish()?;
    let record = TransferRecord {
        begin,
        state,
        next_offset,
        chunks,
        created_ms,
        updated_ms,
        lease_expires_ms,
        participant_boot_nonce,
        semantic_tree_sha256,
        hidden_deployment_id,
        readiness_sha256,
        terminal_error,
    };
    record.validate()?;
    Ok(record)
}

fn checksum(payload: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(CHECKSUM_DOMAIN);
    hash.update(payload);
    format!("{:x}", hash.finalize())
}

fn validate_digest(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn validate_optional_digest(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        Ok(())
    } else {
        validate_digest(value, label)
    }
}

fn validate_text(value: &str, max: usize, empty: bool, label: &str) -> anyhow::Result<()> {
    if (!empty && value.is_empty()) || value.len() > max || value.chars().any(char::is_control) {
        bail!("{label} has invalid length or control characters");
    }
    Ok(())
}

fn put_text(output: &mut Vec<u8>, value: &str, max: usize, empty: bool) -> anyhow::Result<()> {
    validate_text(value, max, empty, "transfer state text")?;
    let len = u16::try_from(value.len()).context("transfer state text length overflow")?;
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8]) -> anyhow::Result<()> {
    let len = u32::try_from(value.len()).context("transfer state byte length overflow")?;
    put_u32(output, len);
    output.extend_from_slice(value);
    Ok(())
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> anyhow::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .context("transfer state field length overflow")?;
        let value = self
            .bytes
            .get(self.offset..end)
            .context("transfer state is truncated")?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("exact length"),
        ))
    }

    fn u64(&mut self) -> anyhow::Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("exact length"),
        ))
    }

    fn bytes(&mut self, max: usize) -> anyhow::Result<&'a [u8]> {
        let len = self.u32()? as usize;
        if len > max {
            bail!("transfer state embedded bytes exceed their fixed limit");
        }
        self.take(len)
    }

    fn text(&mut self, max: usize, empty: bool) -> anyhow::Result<&'a str> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().expect("exact length")) as usize;
        if len > max {
            bail!("transfer state text exceeds its fixed limit");
        }
        let value = std::str::from_utf8(self.take(len)?)?;
        validate_text(value, max, empty, "transfer state text")?;
        Ok(value)
    }

    fn finish(self) -> anyhow::Result<()> {
        if self.offset != self.bytes.len() {
            bail!("transfer state contains trailing bytes");
        }
        Ok(())
    }
}

pub fn chunk_sha256(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}
