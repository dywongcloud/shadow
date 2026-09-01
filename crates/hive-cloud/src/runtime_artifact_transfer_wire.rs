use anyhow::{bail, Context};
use fluid_build::{DeploymentBuildContract, DeploymentBuildSnapshot};
use hive_core::RuntimeArtifactPackageDescriptor;
use sha2::{Digest, Sha256};
use std::str::FromStr;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CONTROL_BYTES: usize = 1024 * 1024;
pub const MAX_ERROR_BYTES: usize = 1024;
pub const MAX_FRAME_BYTES: usize = MAX_CHUNK_BYTES + 64 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 512 * 1024;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// Begin carries two bounded canonical documents (the deployment build
/// snapshot and the final manifest), so it gets its own payload ceiling above
/// the ordinary control-frame cap while staying far under the chunk cap.
pub const MAX_BEGIN_BYTES: usize = MAX_SNAPSHOT_BYTES + MAX_MANIFEST_BYTES + 64 * 1024;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAGIC: &[u8; 8] = b"HIVEATX1";
const GENERATION_DOMAIN: &[u8] = b"hive-runtime-artifact-transfer-generation-v1\0";
const HEADER_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Operation {
    Begin = 1,
    Chunk = 2,
    Query = 3,
    Finalize = 4,
    Abort = 5,
    Prepare = 6,
    Commit = 7,
    Reply = 0x80,
}

impl Operation {
    fn decode(value: u8) -> anyhow::Result<Self> {
        match value {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Chunk),
            3 => Ok(Self::Query),
            4 => Ok(Self::Finalize),
            5 => Ok(Self::Abort),
            6 => Ok(Self::Prepare),
            7 => Ok(Self::Commit),
            0x80 => Ok(Self::Reply),
            _ => bail!("unsupported runtime artifact transfer operation {value}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferKey {
    pub transaction_id: String,
    pub generation_sha256: String,
    pub tenant: String,
    pub coordinator_node: String,
    pub target_node: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeginRequest {
    pub key: TransferKey,
    pub project: String,
    pub project_incarnation: fluid_core::ProjectIncarnation,
    pub snapshot_sha256: String,
    pub snapshot_bytes: Vec<u8>,
    pub normalized_manifest_sha256: String,
    /// The canonical FINAL manifest (every platform mutation already applied),
    /// decoded and re-hashed independently by every receiver — a target never
    /// reconstructs a manifest from its own mutable settings.
    pub manifest_bytes: Vec<u8>,
    /// Deployment lane the whole generation was classified into by the
    /// coordinator's trust resolution. Receivers stage hidden candidates under
    /// this lane; the production ALIAS still only ever moves in the separate
    /// alias transaction.
    pub production: bool,
    /// Display-only deploy author (may be empty; receivers substitute the
    /// platform default). Carries no authority.
    pub creator: String,
    pub target_set_sha256: String,
    pub package: RuntimeArtifactPackageDescriptor,
}

/// Commit echoes the exact prepared authority the coordinator verified, so a
/// receiver can never commit a DIFFERENT prepared incarnation (a re-prepare
/// after restart mints a fresh hidden deployment id and readiness digest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRequest {
    pub key: TransferKey,
    pub hidden_deployment_id: String,
    pub readiness_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkRequest {
    pub key: TransferKey,
    pub offset: u64,
    pub chunk_sha256: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferRequest {
    Begin(BeginRequest),
    Chunk(ChunkRequest),
    Query(TransferKey),
    Finalize(TransferKey),
    Abort(TransferKey),
    Prepare(TransferKey),
    Commit(CommitRequest),
}

impl TransferRequest {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Begin(_) => Operation::Begin,
            Self::Chunk(_) => Operation::Chunk,
            Self::Query(_) => Operation::Query,
            Self::Finalize(_) => Operation::Finalize,
            Self::Abort(_) => Operation::Abort,
            Self::Prepare(_) => Operation::Prepare,
            Self::Commit(_) => Operation::Commit,
        }
    }

    pub fn key(&self) -> &TransferKey {
        match self {
            Self::Begin(request) => &request.key,
            Self::Chunk(request) => &request.key,
            Self::Commit(request) => &request.key,
            Self::Query(key) | Self::Finalize(key) | Self::Abort(key) | Self::Prepare(key) => key,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TransferState {
    Receiving = 1,
    Finalizing = 2,
    Materialized = 3,
    Prepared = 4,
    Committed = 5,
    Aborted = 6,
    Failed = 7,
}

impl TransferState {
    pub fn decode(value: u8) -> anyhow::Result<Self> {
        match value {
            1 => Ok(Self::Receiving),
            2 => Ok(Self::Finalizing),
            3 => Ok(Self::Materialized),
            4 => Ok(Self::Prepared),
            5 => Ok(Self::Committed),
            6 => Ok(Self::Aborted),
            7 => Ok(Self::Failed),
            _ => bail!("invalid runtime artifact transfer state {value}"),
        }
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Aborted | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum ReplyCode {
    Ok = 0,
    Malformed = 1,
    UnsupportedProtocol = 2,
    Unauthorized = 3,
    WrongTarget = 4,
    NotFound = 5,
    Conflict = 6,
    OutOfOrder = 7,
    ChunkConflict = 8,
    ResourceExhausted = 9,
    QueueFull = 10,
    Failed = 11,
    Internal = 12,
}

impl ReplyCode {
    pub fn decode(value: u16) -> anyhow::Result<Self> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::Malformed),
            2 => Ok(Self::UnsupportedProtocol),
            3 => Ok(Self::Unauthorized),
            4 => Ok(Self::WrongTarget),
            5 => Ok(Self::NotFound),
            6 => Ok(Self::Conflict),
            7 => Ok(Self::OutOfOrder),
            8 => Ok(Self::ChunkConflict),
            9 => Ok(Self::ResourceExhausted),
            10 => Ok(Self::QueueFull),
            11 => Ok(Self::Failed),
            12 => Ok(Self::Internal),
            _ => bail!("invalid runtime artifact transfer reply code {value}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferReply {
    pub code: ReplyCode,
    pub transaction_id: String,
    pub generation_sha256: String,
    pub state: TransferState,
    pub next_offset: u64,
    pub package_bytes: u64,
    pub participant_boot_nonce: String,
    pub semantic_tree_sha256: String,
    pub hidden_deployment_id: String,
    pub readiness_sha256: String,
    pub message: String,
}

impl TransferReply {
    pub fn error(code: ReplyCode, key: Option<&TransferKey>, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_ERROR_BYTES {
            message.truncate(MAX_ERROR_BYTES);
        }
        Self {
            code,
            transaction_id: key
                .map(|key| key.transaction_id.clone())
                .unwrap_or_default(),
            generation_sha256: key
                .map(|key| key.generation_sha256.clone())
                .unwrap_or_default(),
            state: TransferState::Failed,
            next_offset: 0,
            package_bytes: 0,
            participant_boot_nonce: String::new(),
            semantic_tree_sha256: String::new(),
            hidden_deployment_id: String::new(),
            readiness_sha256: String::new(),
            message,
        }
    }
}

pub fn encode_request(request: &TransferRequest) -> anyhow::Result<Vec<u8>> {
    let mut payload = Vec::new();
    match request {
        TransferRequest::Begin(request) => encode_begin_payload(request, &mut payload)?,
        TransferRequest::Chunk(request) => {
            encode_key(&request.key, &mut payload)?;
            put_u64(&mut payload, request.offset);
            put_fixed_digest(&mut payload, &request.chunk_sha256, "chunk digest")?;
            put_bytes_u32(
                &mut payload,
                &request.bytes,
                MAX_CHUNK_BYTES,
                "chunk payload",
            )?;
        }
        TransferRequest::Query(key)
        | TransferRequest::Finalize(key)
        | TransferRequest::Abort(key)
        | TransferRequest::Prepare(key) => encode_key(key, &mut payload)?,
        TransferRequest::Commit(request) => {
            encode_key(&request.key, &mut payload)?;
            put_text_u16(
                &mut payload,
                &request.hidden_deployment_id,
                256,
                false,
                "hidden deployment id",
            )?;
            put_fixed_digest(&mut payload, &request.readiness_sha256, "readiness digest")?;
        }
    }
    encode_frame(request.operation(), &payload)
}

pub fn decode_request(frame: &[u8]) -> anyhow::Result<TransferRequest> {
    let (operation, payload) = decode_frame(frame)?;
    if operation == Operation::Reply {
        bail!("runtime artifact transfer request used the reply operation");
    }
    if payload.len() > operation_payload_limit(operation) {
        bail!("runtime artifact transfer frame exceeds its operation limit");
    }
    let mut reader = Reader::new(payload);
    let request = match operation {
        Operation::Begin => TransferRequest::Begin(decode_begin_payload(&mut reader)?),
        Operation::Chunk => {
            let key = decode_key(&mut reader)?;
            let offset = reader.u64()?;
            let chunk_sha256 = reader.fixed_digest("chunk digest")?;
            let bytes = reader.bytes_u32(MAX_CHUNK_BYTES, "chunk payload")?.to_vec();
            TransferRequest::Chunk(ChunkRequest {
                key,
                offset,
                chunk_sha256,
                bytes,
            })
        }
        Operation::Query => TransferRequest::Query(decode_key(&mut reader)?),
        Operation::Finalize => TransferRequest::Finalize(decode_key(&mut reader)?),
        Operation::Abort => TransferRequest::Abort(decode_key(&mut reader)?),
        Operation::Prepare => TransferRequest::Prepare(decode_key(&mut reader)?),
        Operation::Commit => {
            let key = decode_key(&mut reader)?;
            let hidden_deployment_id = reader
                .text_u16(256, false, "hidden deployment id")?
                .to_string();
            let readiness_sha256 = reader.fixed_digest("readiness digest")?;
            TransferRequest::Commit(CommitRequest {
                key,
                hidden_deployment_id,
                readiness_sha256,
            })
        }
        Operation::Reply => unreachable!(),
    };
    reader.finish()?;
    validate_request(&request)?;
    Ok(request)
}

pub fn encode_reply(reply: &TransferReply) -> anyhow::Result<Vec<u8>> {
    validate_optional_digest(&reply.transaction_id, "reply transaction id")?;
    validate_optional_digest(&reply.generation_sha256, "reply generation digest")?;
    validate_optional_digest(&reply.semantic_tree_sha256, "reply semantic tree digest")?;
    validate_optional_digest(&reply.readiness_sha256, "reply readiness digest")?;
    validate_text(
        &reply.participant_boot_nonce,
        128,
        true,
        "participant boot nonce",
    )?;
    validate_text(
        &reply.hidden_deployment_id,
        256,
        true,
        "hidden deployment id",
    )?;
    validate_text(&reply.message, MAX_ERROR_BYTES, true, "reply message")?;
    let mut payload = Vec::new();
    put_u16(&mut payload, reply.code as u16);
    put_text_u16(
        &mut payload,
        &reply.transaction_id,
        64,
        true,
        "transaction id",
    )?;
    put_text_u16(
        &mut payload,
        &reply.generation_sha256,
        64,
        true,
        "generation digest",
    )?;
    payload.push(reply.state as u8);
    put_u64(&mut payload, reply.next_offset);
    put_u64(&mut payload, reply.package_bytes);
    put_text_u16(
        &mut payload,
        &reply.participant_boot_nonce,
        128,
        true,
        "participant boot nonce",
    )?;
    put_text_u16(
        &mut payload,
        &reply.semantic_tree_sha256,
        64,
        true,
        "semantic tree digest",
    )?;
    put_text_u16(
        &mut payload,
        &reply.hidden_deployment_id,
        256,
        true,
        "hidden deployment id",
    )?;
    put_text_u16(
        &mut payload,
        &reply.readiness_sha256,
        64,
        true,
        "readiness digest",
    )?;
    put_text_u16(
        &mut payload,
        &reply.message,
        MAX_ERROR_BYTES,
        true,
        "reply message",
    )?;
    encode_frame(Operation::Reply, &payload)
}

pub fn decode_reply(frame: &[u8]) -> anyhow::Result<TransferReply> {
    let (operation, payload) = decode_frame(frame)?;
    if operation != Operation::Reply {
        bail!("runtime artifact transfer reply used a request operation");
    }
    if payload.len() > MAX_CONTROL_BYTES {
        bail!("runtime artifact transfer reply exceeds the fixed limit");
    }
    let mut reader = Reader::new(payload);
    let reply = TransferReply {
        code: ReplyCode::decode(reader.u16()?)?,
        transaction_id: reader.text_u16(64, true, "transaction id")?.to_string(),
        generation_sha256: reader.text_u16(64, true, "generation digest")?.to_string(),
        state: TransferState::decode(reader.u8()?)?,
        next_offset: reader.u64()?,
        package_bytes: reader.u64()?,
        participant_boot_nonce: reader
            .text_u16(128, true, "participant boot nonce")?
            .to_string(),
        semantic_tree_sha256: reader
            .text_u16(64, true, "semantic tree digest")?
            .to_string(),
        hidden_deployment_id: reader
            .text_u16(256, true, "hidden deployment id")?
            .to_string(),
        readiness_sha256: reader.text_u16(64, true, "readiness digest")?.to_string(),
        message: reader
            .text_u16(MAX_ERROR_BYTES, true, "reply message")?
            .to_string(),
    };
    reader.finish()?;
    validate_optional_digest(&reply.transaction_id, "reply transaction id")?;
    validate_optional_digest(&reply.generation_sha256, "reply generation digest")?;
    validate_optional_digest(&reply.semantic_tree_sha256, "reply semantic tree digest")?;
    validate_optional_digest(&reply.readiness_sha256, "reply readiness digest")?;
    Ok(reply)
}

pub fn validate_request(request: &TransferRequest) -> anyhow::Result<()> {
    validate_key(request.key())?;
    match request {
        TransferRequest::Begin(request) => validate_begin(request),
        TransferRequest::Chunk(request) => {
            if request.bytes.is_empty() || request.bytes.len() > MAX_CHUNK_BYTES {
                bail!("runtime artifact transfer chunk must contain 1..={MAX_CHUNK_BYTES} bytes");
            }
            validate_digest(&request.chunk_sha256, "chunk digest")?;
            let actual = sha256_hex(&request.bytes);
            if actual != request.chunk_sha256 {
                bail!("runtime artifact transfer chunk digest does not match its bytes");
            }
            Ok(())
        }
        TransferRequest::Commit(request) => {
            validate_text(
                &request.hidden_deployment_id,
                256,
                false,
                "hidden deployment id",
            )?;
            validate_digest(&request.readiness_sha256, "readiness digest")
        }
        TransferRequest::Query(_)
        | TransferRequest::Finalize(_)
        | TransferRequest::Abort(_)
        | TransferRequest::Prepare(_) => Ok(()),
    }
}

pub fn validate_begin(request: &BeginRequest) -> anyhow::Result<()> {
    validate_key(&request.key)?;
    validate_text(&request.project, 256, false, "project")?;
    validate_text(&request.creator, 256, true, "creator")?;
    validate_digest(&request.snapshot_sha256, "deployment build snapshot digest")?;
    validate_digest(
        &request.normalized_manifest_sha256,
        "normalized manifest digest",
    )?;
    validate_digest(&request.target_set_sha256, "target set digest")?;
    if request.snapshot_bytes.is_empty() || request.snapshot_bytes.len() > MAX_SNAPSHOT_BYTES {
        bail!(
            "deployment build snapshot canonical bytes must contain 1..={MAX_SNAPSHOT_BYTES} bytes"
        );
    }
    let contract: DeploymentBuildContract = serde_json::from_slice(&request.snapshot_bytes)
        .context("decode canonical deployment build contract")?;
    let snapshot = DeploymentBuildSnapshot::new(contract)
        .context("validate canonical deployment build contract")?;
    if snapshot.digest() != request.snapshot_sha256 {
        bail!("deployment build snapshot digest does not match its canonical contract");
    }
    if snapshot.canonical_contract_bytes()? != request.snapshot_bytes {
        bail!("deployment build snapshot bytes are not canonical");
    }
    if snapshot.contract().authority.normalized_manifest_sha256()
        != request.normalized_manifest_sha256
    {
        bail!("normalized manifest digest differs from deployment build authority");
    }
    validate_manifest_bytes(request)?;
    hive_backend::validate_runtime_artifact_package_descriptor(&request.package)?;
    let computed = generation_sha256(request)?;
    if computed != request.key.generation_sha256 {
        bail!("runtime artifact transfer generation digest does not match its envelope");
    }
    Ok(())
}

/// Decode and independently re-prove the canonical final manifest a Begin
/// envelope carries: exact digest, exact round-trip canonical bytes, exact
/// project identity, and a named sealed image — everything a receiver needs
/// before it may stage this generation without any local reconstruction.
pub fn decode_verified_manifest(request: &BeginRequest) -> anyhow::Result<fluid_core::Manifest> {
    if request.manifest_bytes.is_empty() || request.manifest_bytes.len() > MAX_MANIFEST_BYTES {
        bail!("canonical manifest bytes must contain 1..={MAX_MANIFEST_BYTES} bytes");
    }
    if fluid_core::normalized_manifest_sha256(&request.manifest_bytes)
        != request.normalized_manifest_sha256
    {
        bail!("canonical manifest bytes do not match the normalized manifest digest");
    }
    let manifest: fluid_core::Manifest = serde_json::from_slice(&request.manifest_bytes)
        .context("decode canonical deployment manifest")?;
    if fluid_core::canonical_manifest_bytes(&manifest)? != request.manifest_bytes {
        bail!("deployment manifest bytes are not canonical");
    }
    if manifest.project != request.project {
        bail!("deployment manifest project differs from its transfer envelope");
    }
    if manifest
        .image
        .as_deref()
        .map(str::trim)
        .filter(|image| !image.is_empty())
        .is_none()
    {
        bail!("sealed deployment generation manifest names no runtime image");
    }
    Ok(manifest)
}

fn validate_manifest_bytes(request: &BeginRequest) -> anyhow::Result<()> {
    decode_verified_manifest(request).map(|_| ())
}

pub fn generation_sha256(request: &BeginRequest) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    put_text_u16(
        &mut bytes,
        &request.key.transaction_id,
        64,
        false,
        "transaction id",
    )?;
    put_text_u16(&mut bytes, &request.key.tenant, 256, false, "tenant")?;
    put_text_u16(
        &mut bytes,
        &request.key.coordinator_node,
        256,
        false,
        "coordinator node",
    )?;
    put_text_u16(
        &mut bytes,
        &request.key.target_node,
        256,
        false,
        "target node",
    )?;
    put_text_u16(&mut bytes, &request.project, 256, false, "project")?;
    put_text_u16(
        &mut bytes,
        &request.project_incarnation.path_component(),
        32,
        false,
        "project incarnation",
    )?;
    put_fixed_digest(
        &mut bytes,
        &request.snapshot_sha256,
        "deployment build snapshot digest",
    )?;
    put_bytes_u32(
        &mut bytes,
        &request.snapshot_bytes,
        MAX_SNAPSHOT_BYTES,
        "deployment build snapshot",
    )?;
    put_fixed_digest(
        &mut bytes,
        &request.normalized_manifest_sha256,
        "normalized manifest digest",
    )?;
    put_bytes_u32(
        &mut bytes,
        &request.manifest_bytes,
        MAX_MANIFEST_BYTES,
        "canonical manifest",
    )?;
    bytes.push(request.production as u8);
    put_text_u16(&mut bytes, &request.creator, 256, true, "creator")?;
    put_fixed_digest(&mut bytes, &request.target_set_sha256, "target set digest")?;
    encode_package(&request.package, &mut bytes)?;
    let mut hash = Sha256::new();
    hash.update(GENERATION_DOMAIN);
    hash.update(bytes);
    Ok(format!("{:x}", hash.finalize()))
}

fn validate_key(key: &TransferKey) -> anyhow::Result<()> {
    validate_digest(&key.transaction_id, "transaction id")?;
    validate_digest(&key.generation_sha256, "generation digest")?;
    validate_text(&key.tenant, 256, false, "tenant")?;
    validate_text(&key.coordinator_node, 256, false, "coordinator node")?;
    validate_text(&key.target_node, 256, false, "target node")
}

fn operation_payload_limit(operation: Operation) -> usize {
    match operation {
        Operation::Chunk => MAX_FRAME_BYTES - HEADER_BYTES,
        Operation::Begin => MAX_BEGIN_BYTES,
        _ => MAX_CONTROL_BYTES,
    }
}

fn encode_frame(operation: Operation, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let limit = operation_payload_limit(operation);
    if payload.len() > limit || payload.len() > u32::MAX as usize {
        bail!("runtime artifact transfer frame exceeds its operation limit");
    }
    let mut frame = Vec::with_capacity(HEADER_BYTES + payload.len());
    frame.extend_from_slice(MAGIC);
    put_u16(&mut frame, PROTOCOL_VERSION);
    frame.push(operation as u8);
    frame.push(0);
    put_u32(&mut frame, payload.len() as u32);
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn decode_frame(frame: &[u8]) -> anyhow::Result<(Operation, &[u8])> {
    if frame.len() < HEADER_BYTES {
        bail!("runtime artifact transfer frame is truncated");
    }
    if &frame[..8] != MAGIC {
        bail!("runtime artifact transfer frame has invalid magic");
    }
    let version = u16::from_be_bytes([frame[8], frame[9]]);
    if version != PROTOCOL_VERSION {
        bail!("unsupported runtime artifact transfer protocol {version}");
    }
    let operation = Operation::decode(frame[10])?;
    if frame[11] != 0 {
        bail!("runtime artifact transfer frame has unknown flags");
    }
    let payload_len = u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]) as usize;
    let expected = HEADER_BYTES
        .checked_add(payload_len)
        .context("runtime artifact transfer frame length overflow")?;
    if expected != frame.len() {
        bail!("runtime artifact transfer frame length or trailing bytes are invalid");
    }
    Ok((operation, &frame[HEADER_BYTES..]))
}

fn encode_begin_payload(request: &BeginRequest, output: &mut Vec<u8>) -> anyhow::Result<()> {
    validate_begin(request)?;
    encode_key(&request.key, output)?;
    put_text_u16(output, &request.project, 256, false, "project")?;
    put_text_u16(
        output,
        &request.project_incarnation.path_component(),
        32,
        false,
        "project incarnation",
    )?;
    put_fixed_digest(
        output,
        &request.snapshot_sha256,
        "deployment build snapshot digest",
    )?;
    put_bytes_u32(
        output,
        &request.snapshot_bytes,
        MAX_SNAPSHOT_BYTES,
        "deployment build snapshot",
    )?;
    put_fixed_digest(
        output,
        &request.normalized_manifest_sha256,
        "normalized manifest digest",
    )?;
    put_bytes_u32(
        output,
        &request.manifest_bytes,
        MAX_MANIFEST_BYTES,
        "canonical manifest",
    )?;
    output.push(request.production as u8);
    put_text_u16(output, &request.creator, 256, true, "creator")?;
    put_fixed_digest(output, &request.target_set_sha256, "target set digest")?;
    encode_package(&request.package, output)
}

fn decode_begin_payload(reader: &mut Reader<'_>) -> anyhow::Result<BeginRequest> {
    let key = decode_key(reader)?;
    let project = reader.text_u16(256, false, "project")?.to_string();
    let incarnation = reader
        .text_u16(32, false, "project incarnation")?
        .to_string();
    let project_incarnation = fluid_core::ProjectIncarnation::from_str(&incarnation)
        .context("invalid project incarnation")?;
    let snapshot_sha256 = reader.fixed_digest("deployment build snapshot digest")?;
    let snapshot_bytes = reader
        .bytes_u32(MAX_SNAPSHOT_BYTES, "deployment build snapshot")?
        .to_vec();
    let normalized_manifest_sha256 = reader.fixed_digest("normalized manifest digest")?;
    let manifest_bytes = reader
        .bytes_u32(MAX_MANIFEST_BYTES, "canonical manifest")?
        .to_vec();
    let production = match reader.u8()? {
        0 => false,
        1 => true,
        other => bail!("invalid runtime artifact transfer lane flag {other}"),
    };
    let creator = reader.text_u16(256, true, "creator")?.to_string();
    let target_set_sha256 = reader.fixed_digest("target set digest")?;
    let package = decode_package(reader)?;
    Ok(BeginRequest {
        key,
        project,
        project_incarnation,
        snapshot_sha256,
        snapshot_bytes,
        normalized_manifest_sha256,
        manifest_bytes,
        production,
        creator,
        target_set_sha256,
        package,
    })
}

fn encode_key(key: &TransferKey, output: &mut Vec<u8>) -> anyhow::Result<()> {
    validate_key(key)?;
    put_fixed_digest(output, &key.transaction_id, "transaction id")?;
    put_fixed_digest(output, &key.generation_sha256, "generation digest")?;
    put_text_u16(output, &key.tenant, 256, false, "tenant")?;
    put_text_u16(
        output,
        &key.coordinator_node,
        256,
        false,
        "coordinator node",
    )?;
    put_text_u16(output, &key.target_node, 256, false, "target node")
}

fn decode_key(reader: &mut Reader<'_>) -> anyhow::Result<TransferKey> {
    Ok(TransferKey {
        transaction_id: reader.fixed_digest("transaction id")?,
        generation_sha256: reader.fixed_digest("generation digest")?,
        tenant: reader.text_u16(256, false, "tenant")?.to_string(),
        coordinator_node: reader.text_u16(256, false, "coordinator node")?.to_string(),
        target_node: reader.text_u16(256, false, "target node")?.to_string(),
    })
}

fn encode_package(
    package: &RuntimeArtifactPackageDescriptor,
    output: &mut Vec<u8>,
) -> anyhow::Result<()> {
    hive_backend::validate_runtime_artifact_package_descriptor(package)?;
    put_u16(output, package.protocol);
    put_fixed_digest(output, &package.package_sha256, "package digest")?;
    put_fixed_digest(
        output,
        &package.semantic_tree_sha256,
        "semantic tree digest",
    )?;
    put_u64(output, package.package_bytes);
    put_u64(output, package.logical_bytes);
    put_u64(output, package.materialized_bytes);
    put_u64(output, package.entries);
    put_text_u16(output, &package.app_rel, 4096, true, "application path")?;
    if package.include_rel.len() > 100_000 {
        bail!("runtime artifact package has too many included paths");
    }
    put_u32(output, package.include_rel.len() as u32);
    for path in &package.include_rel {
        put_text_u16(output, path, 4096, false, "included path")?;
    }
    Ok(())
}

fn decode_package(reader: &mut Reader<'_>) -> anyhow::Result<RuntimeArtifactPackageDescriptor> {
    let protocol = reader.u16()?;
    let package_sha256 = reader.fixed_digest("package digest")?;
    let semantic_tree_sha256 = reader.fixed_digest("semantic tree digest")?;
    let package_bytes = reader.u64()?;
    let logical_bytes = reader.u64()?;
    let materialized_bytes = reader.u64()?;
    let entries = reader.u64()?;
    let app_rel = reader.text_u16(4096, true, "application path")?.to_string();
    let count = reader.u32()? as usize;
    if count > 100_000 {
        bail!("runtime artifact package has too many included paths");
    }
    let mut include_rel = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        include_rel.push(reader.text_u16(4096, false, "included path")?.to_string());
    }
    let package = RuntimeArtifactPackageDescriptor {
        protocol,
        package_sha256,
        semantic_tree_sha256,
        package_bytes,
        logical_bytes,
        materialized_bytes,
        entries,
        app_rel,
        include_rel,
    };
    hive_backend::validate_runtime_artifact_package_descriptor(&package)?;
    Ok(package)
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
    if (!empty && value.is_empty())
        || value.len() > max.min(MAX_TEXT_BYTES)
        || value.chars().any(char::is_control)
    {
        bail!("{label} has invalid length or control characters");
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}

fn put_fixed_digest(output: &mut Vec<u8>, value: &str, label: &str) -> anyhow::Result<()> {
    validate_digest(value, label)?;
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_text_u16(
    output: &mut Vec<u8>,
    value: &str,
    max: usize,
    empty: bool,
    label: &str,
) -> anyhow::Result<()> {
    validate_text(value, max, empty, label)?;
    let len =
        u16::try_from(value.len()).context("runtime artifact transfer text length overflow")?;
    put_u16(output, len);
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_bytes_u32(
    output: &mut Vec<u8>,
    value: &[u8],
    max: usize,
    label: &str,
) -> anyhow::Result<()> {
    if value.len() > max {
        bail!("{label} exceeds its fixed byte limit");
    }
    let len =
        u32::try_from(value.len()).context("runtime artifact transfer byte length overflow")?;
    put_u32(output, len);
    output.extend_from_slice(value);
    Ok(())
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_be_bytes());
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
            .context("runtime artifact transfer field length overflow")?;
        let value = self
            .bytes
            .get(self.offset..end)
            .context("runtime artifact transfer frame is truncated")?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> anyhow::Result<u16> {
        let bytes: [u8; 2] = self.take(2)?.try_into().expect("exact length");
        Ok(u16::from_be_bytes(bytes))
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("exact length");
        Ok(u32::from_be_bytes(bytes))
    }

    fn u64(&mut self) -> anyhow::Result<u64> {
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("exact length");
        Ok(u64::from_be_bytes(bytes))
    }

    fn fixed_digest(&mut self, label: &str) -> anyhow::Result<String> {
        let value =
            std::str::from_utf8(self.take(64)?).with_context(|| format!("{label} is not UTF-8"))?;
        validate_digest(value, label)?;
        Ok(value.to_string())
    }

    fn text_u16(&mut self, max: usize, empty: bool, label: &str) -> anyhow::Result<&'a str> {
        let len = self.u16()? as usize;
        if len > max.min(MAX_TEXT_BYTES) {
            bail!("{label} exceeds its fixed text limit");
        }
        let value = std::str::from_utf8(self.take(len)?)
            .with_context(|| format!("{label} is not UTF-8"))?;
        validate_text(value, max, empty, label)?;
        Ok(value)
    }

    fn bytes_u32(&mut self, max: usize, label: &str) -> anyhow::Result<&'a [u8]> {
        let len = self.u32()? as usize;
        if len > max {
            bail!("{label} exceeds its fixed byte limit");
        }
        self.take(len)
    }

    fn finish(self) -> anyhow::Result<()> {
        if self.offset != self.bytes.len() {
            bail!("runtime artifact transfer frame contains trailing bytes");
        }
        Ok(())
    }
}
