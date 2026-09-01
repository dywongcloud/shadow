use anyhow::{bail, Context};
use bytes::Bytes;
use futures::{stream, StreamExt};
use sha2::{Digest, Sha256};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::Duration;

use crate::runtime_artifact_transfer_wire::{
    self as wire, BeginRequest, ChunkRequest, Operation, ReplyCode, TransferKey, TransferReply,
    TransferRequest, TransferState,
};
use crate::schedule::Target;
use crate::state::CloudState;

const TARGET_SET_DOMAIN: &[u8] = b"hive-runtime-artifact-transfer-target-set-v1\0";
const TRANSFER_PATH: &str = "/v1/runtime-artifact-transfer/v1";
const CONTENT_TYPE: &str = "application/vnd.hive.runtime-artifact-transfer.v1";
const DEFAULT_TARGET_CONCURRENCY: usize = 2;
const MAX_TARGET_CONCURRENCY: usize = 8;
const MAX_TARGETS: usize = 64;
const MAX_ATTEMPTS: usize = 4;
const REQUEST_TIMEOUT_SECS: u64 = 60;
const SERVICE_TOKEN_TTL_SECS: i64 = 180;

#[derive(Clone, Debug)]
pub struct TransferGeneration {
    transaction_id: String,
    project: String,
    project_incarnation: fluid_core::ProjectIncarnation,
    tenant: String,
    coordinator_node: String,
    snapshot: fluid_build::DeploymentBuildSnapshot,
    snapshot_bytes: Vec<u8>,
    manifest_bytes: Vec<u8>,
    production: bool,
    creator: String,
    target_set_sha256: String,
    targets: Vec<Target>,
}

#[derive(Clone, Debug)]
pub struct TargetMaterializationReceipt {
    pub target_node: String,
    pub transaction_id: String,
    pub generation_sha256: String,
    pub participant_boot_nonce: String,
    pub package_sha256: String,
    pub semantic_tree_sha256: String,
    pub state: TransferState,
}

/// Proof one target holds a HIDDEN, launched, readiness-proven candidate for
/// this exact generation. The `hidden_deployment_id`/`readiness_sha256` pair
/// is what Commit must echo back — a receiver refuses to commit any other
/// prepared incarnation, and `participant_boot_nonce` pins the proof to the
/// exact process that holds the armed staged handle.
#[derive(Clone, Debug)]
pub struct TargetPreparationReceipt {
    pub target_node: String,
    pub transaction_id: String,
    pub generation_sha256: String,
    pub participant_boot_nonce: String,
    pub hidden_deployment_id: String,
    pub readiness_sha256: String,
}

/// Proof one target durably committed (published) its prepared candidate.
#[derive(Clone, Debug)]
pub struct TargetCommitReceipt {
    pub target_node: String,
    pub transaction_id: String,
    pub generation_sha256: String,
    pub hidden_deployment_id: String,
    pub readiness_sha256: String,
}

impl TransferGeneration {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transaction_id: impl Into<String>,
        project: impl Into<String>,
        project_incarnation: fluid_core::ProjectIncarnation,
        tenant: impl Into<String>,
        coordinator_node: impl Into<String>,
        snapshot: fluid_build::DeploymentBuildSnapshot,
        manifest_bytes: Vec<u8>,
        production: bool,
        creator: impl Into<String>,
        mut targets: Vec<Target>,
    ) -> anyhow::Result<Self> {
        let transaction_id = transaction_id.into();
        validate_digest(&transaction_id, "transfer transaction id")?;
        let project = project.into();
        validate_text(&project, 256, "transfer project")?;
        let tenant = crate::admin::norm(&tenant.into()).to_string();
        validate_text(&tenant, 256, "transfer tenant")?;
        let coordinator_node = coordinator_node.into();
        validate_text(&coordinator_node, 256, "transfer coordinator node")?;
        let creator = creator.into();
        snapshot
            .verify()
            .context("verify deployment build snapshot before transfer")?;
        let snapshot_bytes = snapshot
            .canonical_contract_bytes()
            .context("encode deployment build snapshot before transfer")?;
        if fluid_core::normalized_manifest_sha256(&manifest_bytes)
            != snapshot.contract().authority.normalized_manifest_sha256()
        {
            bail!("canonical manifest bytes do not match the sealed build authority");
        }

        if targets.is_empty() || targets.len() > MAX_TARGETS {
            bail!("runtime artifact transfer target set must contain 1..={MAX_TARGETS} nodes");
        }
        targets.sort_by(|left, right| left.node.cmp(&right.node));
        for (index, target) in targets.iter().enumerate() {
            validate_text(&target.node, 256, "transfer target node")?;
            if index > 0 && targets[index - 1].node == target.node {
                bail!(
                    "runtime artifact transfer target set contains duplicate node {:?}",
                    target.node
                );
            }
            if target.node != coordinator_node && target.admin.is_none() && target.iroh.is_none() {
                bail!(
                    "runtime artifact transfer target {:?} has no HTTP or iroh route",
                    target.node
                );
            }
            if let Some(admin) = &target.admin {
                validate_text(admin, 16 * 1024, "transfer HTTP route")?;
                if !admin.starts_with("http://") && !admin.starts_with("https://") {
                    bail!("runtime artifact transfer HTTP route must be absolute");
                }
            }
            if let Some((peer_id, address)) = &target.iroh {
                validate_text(peer_id, 256, "transfer iroh peer id")?;
                validate_text(address, 16 * 1024, "transfer iroh address")?;
            }
        }
        let target_set_sha256 = target_set_sha256(&targets)?;
        Ok(Self {
            transaction_id,
            project,
            project_incarnation,
            tenant,
            coordinator_node,
            snapshot,
            snapshot_bytes,
            manifest_bytes,
            production,
            creator,
            target_set_sha256,
            targets,
        })
    }

    pub fn target_set_sha256(&self) -> &str {
        &self.target_set_sha256
    }

    fn begin_for(
        &self,
        target_node: &str,
        package: &hive_core::RuntimeArtifactPackageDescriptor,
    ) -> anyhow::Result<BeginRequest> {
        let mut request = BeginRequest {
            key: TransferKey {
                transaction_id: self.transaction_id.clone(),
                generation_sha256: "0".repeat(64),
                tenant: self.tenant.clone(),
                coordinator_node: self.coordinator_node.clone(),
                target_node: target_node.to_string(),
            },
            project: self.project.clone(),
            project_incarnation: self.project_incarnation,
            snapshot_sha256: self.snapshot.digest().to_string(),
            snapshot_bytes: self.snapshot_bytes.clone(),
            normalized_manifest_sha256: self
                .snapshot
                .contract()
                .authority
                .normalized_manifest_sha256()
                .to_string(),
            manifest_bytes: self.manifest_bytes.clone(),
            production: self.production,
            creator: self.creator.clone(),
            target_set_sha256: self.target_set_sha256.clone(),
            package: package.clone(),
        };
        request.key.generation_sha256 = wire::generation_sha256(&request)?;
        wire::validate_begin(&request)?;
        Ok(request)
    }

    fn key_for(
        &self,
        target_node: &str,
        package: &hive_core::RuntimeArtifactPackageDescriptor,
    ) -> anyhow::Result<TransferKey> {
        Ok(self.begin_for(target_node, package)?.key)
    }

    fn remote_targets(&self) -> impl Iterator<Item = &Target> {
        self.targets
            .iter()
            .filter(move |target| target.node != self.coordinator_node)
    }
}

pub async fn send_verified_runtime_artifact(
    cloud: &Arc<CloudState>,
    generation: TransferGeneration,
    package: hive_backend::VerifiedRuntimeArtifactPackage,
) -> anyhow::Result<Vec<TargetMaterializationReceipt>> {
    if generation.coordinator_node != cloud.node_name {
        bail!(
            "runtime artifact transfer coordinator {:?} does not match local node {:?}",
            generation.coordinator_node,
            cloud.node_name
        );
    }
    let concurrency = target_concurrency()?;
    let descriptor = package.descriptor().clone();
    hive_backend::validate_runtime_artifact_package_descriptor(&descriptor)?;
    let (package_file, verified_descriptor) = package.into_parts();
    if descriptor != verified_descriptor {
        bail!("verified runtime artifact package descriptor changed before transfer");
    }
    let package_file = Arc::new(package_file);

    let mut sends = Vec::new();
    for target in &generation.targets {
        if target.node == generation.coordinator_node {
            continue;
        }
        sends.push((
            target.clone(),
            generation.begin_for(&target.node, &descriptor)?,
        ));
    }
    if sends.is_empty() {
        return Ok(Vec::new());
    }

    let results = stream::iter(sends)
        .map(|(target, begin)| {
            let cloud = cloud.clone();
            let package_file = package_file.clone();
            async move { transfer_target(&cloud, target, begin, package_file).await }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

    let mut receipts = Vec::new();
    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok(receipt) => receipts.push(receipt),
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    receipts.sort_by(|left, right| left.target_node.cmp(&right.target_node));
    failures.sort();
    if !failures.is_empty() {
        bail!(
            "runtime artifact transfer did not materialize every target: {}",
            failures.join("; ")
        );
    }
    Ok(receipts)
}

/// Ask every remote target of one materialized generation to PREPARE: deliver
/// the exact artifact to its backend, stage the canonical manifest as a hidden
/// candidate, launch it, and prove bounded readiness. Fails unless EVERY
/// remote target returns a prepared receipt — partial preparation never
/// publishes anything, and the coordinator can abort or retry the whole set.
pub async fn prepare_transferred_targets(
    cloud: &Arc<CloudState>,
    generation: &TransferGeneration,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
) -> anyhow::Result<Vec<TargetPreparationReceipt>> {
    if generation.coordinator_node != cloud.node_name {
        bail!(
            "runtime artifact transfer coordinator {:?} does not match local node {:?}",
            generation.coordinator_node,
            cloud.node_name
        );
    }
    let concurrency = target_concurrency()?;
    let remote: Vec<Target> = generation.remote_targets().cloned().collect();
    let results = stream::iter(remote)
        .map(|target| {
            let cloud = cloud.clone();
            let key = generation.key_for(&target.node, package);
            let package = package.clone();
            async move {
                let key = key?;
                prepare_target(&cloud, &target, &key, &package).await
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    let mut receipts = Vec::new();
    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok(receipt) => receipts.push(receipt),
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    receipts.sort_by(|left, right| left.target_node.cmp(&right.target_node));
    failures.sort();
    if !failures.is_empty() {
        bail!(
            "runtime artifact transfer did not prepare every target: {}",
            failures.join("; ")
        );
    }
    Ok(receipts)
}

async fn prepare_target(
    cloud: &Arc<CloudState>,
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
) -> anyhow::Result<TargetPreparationReceipt> {
    let frame = Bytes::from(wire::encode_request(&TransferRequest::Prepare(
        key.clone(),
    ))?);
    let mut errors = Vec::new();
    for attempt in 0..MAX_ATTEMPTS {
        match exchange(
            cloud,
            target,
            key,
            package,
            Operation::Prepare,
            frame.clone(),
        )
        .await
        {
            Ok(reply) if reply.code == ReplyCode::Ok && reply.state == TransferState::Prepared => {
                return prepared_receipt(target, key, reply);
            }
            Ok(reply)
                if matches!(
                    reply.code,
                    // `Failed` on Prepare is RETRYABLE by contract: the
                    // receiver keeps the record Materialized and names the
                    // launch/readiness failure in the message.
                    ReplyCode::QueueFull | ReplyCode::Internal | ReplyCode::Failed
                ) =>
            {
                errors.push(reply.message);
            }
            Ok(reply) => return refused(target, "prepare", reply),
            Err(error) => errors.push(format!("prepare transport: {error:#}")),
        }
        match query(cloud, target, key, package).await {
            Ok(reply) if reply.code == ReplyCode::Ok && reply.state == TransferState::Prepared => {
                return prepared_receipt(target, key, reply);
            }
            Ok(reply)
                if reply.code == ReplyCode::QueueFull
                    || (reply.code == ReplyCode::Ok
                        && reply.state == TransferState::Materialized) =>
            {
                errors.push(reply.message);
            }
            Ok(reply) => return refused(target, "prepare recovery query", reply),
            Err(error) => errors.push(format!("prepare recovery query: {error:#}")),
        }
        retry_delay(attempt).await;
    }
    bail!(
        "target {} did not prepare the runtime artifact generation after {MAX_ATTEMPTS} attempts: {}",
        target.node,
        errors.join(" | ")
    )
}

fn prepared_receipt(
    target: &Target,
    key: &TransferKey,
    reply: TransferReply,
) -> anyhow::Result<TargetPreparationReceipt> {
    if reply.hidden_deployment_id.is_empty()
        || reply.readiness_sha256.is_empty()
        || reply.participant_boot_nonce.is_empty()
    {
        bail!(
            "target {} returned a prepared state without hidden deployment readiness authority",
            target.node
        );
    }
    Ok(TargetPreparationReceipt {
        target_node: target.node.clone(),
        transaction_id: key.transaction_id.clone(),
        generation_sha256: key.generation_sha256.clone(),
        participant_boot_nonce: reply.participant_boot_nonce,
        hidden_deployment_id: reply.hidden_deployment_id,
        readiness_sha256: reply.readiness_sha256,
    })
}

/// Commit every prepared remote target of one generation, echoing each
/// target's EXACT prepared authority. Requires one receipt per remote target
/// — a coordinator that cannot show a full prepared set has no business
/// publishing anything.
pub async fn commit_prepared_targets(
    cloud: &Arc<CloudState>,
    generation: &TransferGeneration,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
    prepared: &[TargetPreparationReceipt],
) -> anyhow::Result<Vec<TargetCommitReceipt>> {
    if generation.coordinator_node != cloud.node_name {
        bail!(
            "runtime artifact transfer coordinator {:?} does not match local node {:?}",
            generation.coordinator_node,
            cloud.node_name
        );
    }
    let mut by_node = std::collections::BTreeMap::new();
    for receipt in prepared {
        if by_node
            .insert(receipt.target_node.as_str(), receipt)
            .is_some()
        {
            bail!(
                "duplicate prepared receipt for target {:?}",
                receipt.target_node
            );
        }
    }
    let mut sends = Vec::new();
    for target in generation.remote_targets() {
        let Some(receipt) = by_node.get(target.node.as_str()) else {
            bail!(
                "target {:?} has no prepared receipt; the generation cannot commit",
                target.node
            );
        };
        let key = generation.key_for(&target.node, package)?;
        if receipt.transaction_id != key.transaction_id
            || receipt.generation_sha256 != key.generation_sha256
        {
            bail!(
                "prepared receipt for target {:?} names another transaction or generation",
                target.node
            );
        }
        sends.push((target.clone(), key, (*receipt).clone()));
    }
    let concurrency = target_concurrency()?;
    let results = stream::iter(sends)
        .map(|(target, key, receipt)| {
            let cloud = cloud.clone();
            let package = package.clone();
            async move { commit_target(&cloud, &target, &key, &package, &receipt).await }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    let mut receipts = Vec::new();
    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok(receipt) => receipts.push(receipt),
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    receipts.sort_by(|left, right| left.target_node.cmp(&right.target_node));
    failures.sort();
    if !failures.is_empty() {
        bail!(
            "runtime artifact transfer did not commit every prepared target: {}",
            failures.join("; ")
        );
    }
    Ok(receipts)
}

async fn commit_target(
    cloud: &Arc<CloudState>,
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
    prepared: &TargetPreparationReceipt,
) -> anyhow::Result<TargetCommitReceipt> {
    let frame = Bytes::from(wire::encode_request(&TransferRequest::Commit(
        wire::CommitRequest {
            key: key.clone(),
            hidden_deployment_id: prepared.hidden_deployment_id.clone(),
            readiness_sha256: prepared.readiness_sha256.clone(),
        },
    ))?);
    let mut errors = Vec::new();
    for attempt in 0..MAX_ATTEMPTS {
        match exchange(
            cloud,
            target,
            key,
            package,
            Operation::Commit,
            frame.clone(),
        )
        .await
        {
            Ok(reply) if reply.code == ReplyCode::Ok && reply.state == TransferState::Committed => {
                return committed_receipt(target, key, prepared, reply);
            }
            Ok(reply)
                if matches!(
                    reply.code,
                    ReplyCode::QueueFull | ReplyCode::Internal | ReplyCode::Failed
                ) =>
            {
                errors.push(reply.message);
            }
            Ok(reply) => return refused(target, "commit", reply),
            Err(error) => errors.push(format!("commit transport: {error:#}")),
        }
        match query(cloud, target, key, package).await {
            Ok(reply) if reply.code == ReplyCode::Ok && reply.state == TransferState::Committed => {
                return committed_receipt(target, key, prepared, reply);
            }
            Ok(reply)
                if reply.code == ReplyCode::QueueFull
                    || (reply.code == ReplyCode::Ok && reply.state == TransferState::Prepared) =>
            {
                errors.push(reply.message);
            }
            Ok(reply) => return refused(target, "commit recovery query", reply),
            Err(error) => errors.push(format!("commit recovery query: {error:#}")),
        }
        retry_delay(attempt).await;
    }
    bail!(
        "target {} did not commit the prepared runtime artifact generation after {MAX_ATTEMPTS} attempts: {}",
        target.node,
        errors.join(" | ")
    )
}

fn committed_receipt(
    target: &Target,
    key: &TransferKey,
    prepared: &TargetPreparationReceipt,
    reply: TransferReply,
) -> anyhow::Result<TargetCommitReceipt> {
    if reply.hidden_deployment_id != prepared.hidden_deployment_id
        || reply.readiness_sha256 != prepared.readiness_sha256
    {
        bail!(
            "target {} committed a different prepared candidate than this generation proved",
            target.node
        );
    }
    Ok(TargetCommitReceipt {
        target_node: target.node.clone(),
        transaction_id: key.transaction_id.clone(),
        generation_sha256: key.generation_sha256.clone(),
        hidden_deployment_id: reply.hidden_deployment_id,
        readiness_sha256: reply.readiness_sha256,
    })
}

/// Best-effort abort of every remote participant of one generation: prepared
/// candidates roll back (their hidden staging is dropped), committed targets
/// refuse the abort and keep serving, unknown transactions are a no-op. Never
/// fails the caller — a target that cannot be told to abort is reaped by its
/// own lease expiry.
pub async fn abort_generation_targets(
    cloud: &Arc<CloudState>,
    generation: &TransferGeneration,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
) {
    for target in generation.remote_targets() {
        let key = match generation.key_for(&target.node, package) {
            Ok(key) => key,
            Err(error) => {
                tracing::warn!(target = %target.node, %error, "runtime artifact abort key derivation failed");
                continue;
            }
        };
        let frame = match wire::encode_request(&TransferRequest::Abort(key.clone())) {
            Ok(frame) => Bytes::from(frame),
            Err(error) => {
                tracing::warn!(target = %target.node, %error, "runtime artifact abort frame encoding failed");
                continue;
            }
        };
        if let Err(error) = exchange(cloud, target, &key, package, Operation::Abort, frame).await {
            tracing::warn!(target = %target.node, %error, "runtime artifact transfer abort did not reach the target");
        }
    }
}

async fn transfer_target(
    cloud: &Arc<CloudState>,
    target: Target,
    begin: BeginRequest,
    package_file: Arc<std::fs::File>,
) -> anyhow::Result<TargetMaterializationReceipt> {
    let key = begin.key.clone();
    let package = begin.package.clone();
    let begin_frame = Bytes::from(wire::encode_request(&TransferRequest::Begin(begin))?);
    let mut reply = begin_or_resume(cloud, &target, &key, &package, begin_frame).await?;
    if materialized(reply.state) {
        return receipt(&target, &key, &package, reply);
    }
    if reply.state != TransferState::Receiving {
        bail!(
            "target {} resumed runtime artifact transfer in unusable state {:?}",
            target.node,
            reply.state
        );
    }

    let mut next_offset = reply.next_offset;
    while next_offset < package.package_bytes {
        let remaining = package.package_bytes - next_offset;
        let length = remaining.min(wire::MAX_CHUNK_BYTES as u64) as usize;
        let bytes = read_package_at(package_file.clone(), next_offset, length).await?;
        let end = next_offset
            .checked_add(bytes.len() as u64)
            .context("runtime artifact transfer chunk end overflow")?;
        let chunk = TransferRequest::Chunk(ChunkRequest {
            key: key.clone(),
            offset: next_offset,
            chunk_sha256: wire::sha256_hex(&bytes),
            bytes,
        });
        let frame = Bytes::from(wire::encode_request(&chunk)?);
        drop(chunk);
        next_offset =
            persist_chunk(cloud, &target, &key, &package, next_offset, end, frame).await?;
    }

    reply = finalize_target(cloud, &target, &key, &package).await?;
    receipt(&target, &key, &package, reply)
}

async fn begin_or_resume(
    cloud: &Arc<CloudState>,
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
    begin_frame: Bytes,
) -> anyhow::Result<TransferReply> {
    let mut errors = Vec::new();
    for attempt in 0..MAX_ATTEMPTS {
        match exchange(
            cloud,
            target,
            key,
            package,
            Operation::Begin,
            begin_frame.clone(),
        )
        .await
        {
            Ok(reply) if reply.code == ReplyCode::Ok => return Ok(reply),
            Ok(reply) if reply.code == ReplyCode::QueueFull => {
                errors.push(reply.message);
            }
            Ok(reply) => return refused(target, "begin", reply),
            Err(error) => errors.push(format!("begin transport: {error:#}")),
        }
        match query(cloud, target, key, package).await {
            Ok(reply) if reply.code == ReplyCode::Ok => return Ok(reply),
            Ok(reply) if matches!(reply.code, ReplyCode::NotFound | ReplyCode::QueueFull) => {
                errors.push(reply.message);
            }
            Ok(reply) => return refused(target, "begin recovery query", reply),
            Err(error) => errors.push(format!("begin recovery query: {error:#}")),
        }
        retry_delay(attempt).await;
    }
    bail!(
        "target {} could not begin or recover runtime artifact transfer after {MAX_ATTEMPTS} attempts: {}",
        target.node,
        errors.join(" | ")
    )
}

async fn persist_chunk(
    cloud: &Arc<CloudState>,
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
    offset: u64,
    end: u64,
    frame: Bytes,
) -> anyhow::Result<u64> {
    let mut errors = Vec::new();
    for attempt in 0..MAX_ATTEMPTS {
        match exchange(cloud, target, key, package, Operation::Chunk, frame.clone()).await {
            Ok(reply) if reply.code == ReplyCode::Ok && reply.next_offset >= end => {
                return Ok(reply.next_offset)
            }
            Ok(reply)
                if matches!(
                    reply.code,
                    ReplyCode::QueueFull | ReplyCode::OutOfOrder | ReplyCode::Internal
                ) =>
            {
                errors.push(reply.message);
            }
            Ok(reply) if reply.code == ReplyCode::Ok => {
                errors.push(format!(
                    "chunk acknowledgement stopped at {} before {end}",
                    reply.next_offset
                ));
            }
            Ok(reply) => return refused(target, "chunk", reply),
            Err(error) => {
                errors.push(format!("chunk transport: {error:#}"));
            }
        }
        match query(cloud, target, key, package).await {
            Ok(reply) if reply.code == ReplyCode::Ok && reply.next_offset >= end => {
                return Ok(reply.next_offset)
            }
            Ok(reply) if reply.code == ReplyCode::Ok && reply.next_offset == offset => {
                errors.push("chunk is not yet durable".to_string());
            }
            Ok(reply) if reply.code == ReplyCode::QueueFull => errors.push(reply.message),
            Ok(reply) if reply.code == ReplyCode::Ok => {
                bail!(
                    "target {} reported impossible partial chunk progress {} for range {offset}..{end}",
                    target.node,
                    reply.next_offset
                )
            }
            Ok(reply) => return refused(target, "chunk recovery query", reply),
            Err(error) => errors.push(format!("chunk recovery query: {error:#}")),
        }
        retry_delay(attempt).await;
    }
    bail!(
        "target {} did not persist runtime artifact chunk {offset}..{end} after {MAX_ATTEMPTS} attempts: {}",
        target.node,
        errors.join(" | ")
    )
}

async fn finalize_target(
    cloud: &Arc<CloudState>,
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
) -> anyhow::Result<TransferReply> {
    let frame = Bytes::from(wire::encode_request(&TransferRequest::Finalize(
        key.clone(),
    ))?);
    let mut errors = Vec::new();
    for attempt in 0..MAX_ATTEMPTS {
        match exchange(
            cloud,
            target,
            key,
            package,
            Operation::Finalize,
            frame.clone(),
        )
        .await
        {
            Ok(reply) if reply.code == ReplyCode::Ok && materialized(reply.state) => {
                return Ok(reply)
            }
            Ok(reply)
                if matches!(reply.code, ReplyCode::QueueFull | ReplyCode::OutOfOrder)
                    || (reply.code == ReplyCode::Ok
                        && matches!(
                            reply.state,
                            TransferState::Receiving | TransferState::Finalizing
                        )) =>
            {
                errors.push(reply.message);
            }
            Ok(reply) => return refused(target, "finalize", reply),
            Err(error) => errors.push(format!("finalize transport: {error:#}")),
        }
        match query(cloud, target, key, package).await {
            Ok(reply) if reply.code == ReplyCode::Ok && materialized(reply.state) => {
                return Ok(reply)
            }
            Ok(reply)
                if reply.code == ReplyCode::QueueFull
                    || (reply.code == ReplyCode::Ok
                        && matches!(
                            reply.state,
                            TransferState::Receiving | TransferState::Finalizing
                        )) =>
            {
                errors.push(reply.message);
            }
            Ok(reply) => return refused(target, "finalize recovery query", reply),
            Err(error) => errors.push(format!("finalize recovery query: {error:#}")),
        }
        retry_delay(attempt).await;
    }
    bail!(
        "target {} did not materialize runtime artifact after {MAX_ATTEMPTS} finalize attempts: {}",
        target.node,
        errors.join(" | ")
    )
}

async fn query(
    cloud: &Arc<CloudState>,
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
) -> anyhow::Result<TransferReply> {
    let frame = Bytes::from(wire::encode_request(&TransferRequest::Query(key.clone()))?);
    exchange(cloud, target, key, package, Operation::Query, frame).await
}

async fn exchange(
    cloud: &Arc<CloudState>,
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
    operation: Operation,
    frame: Bytes,
) -> anyhow::Result<TransferReply> {
    let operation_name = operation_name(operation);
    let mut unavailable = Vec::new();
    if let Some(admin) = &target.admin {
        let token = service_token(cloud, key)?;
        match http_exchange(cloud, admin, operation_name, &token, frame.clone()).await {
            TransportAttempt::Reply(reply) => return validate_reply(target, key, package, reply),
            TransportAttempt::Unavailable(reason) => unavailable.push(format!("HTTP: {reason}")),
        }
    }
    if let Some((peer_id, address)) = &target.iroh {
        let token = service_token(cloud, key)?;
        match iroh_exchange(
            cloud,
            peer_id,
            address,
            operation_name,
            &token,
            frame.as_ref(),
        )
        .await
        {
            TransportAttempt::Reply(reply) => return validate_reply(target, key, package, reply),
            TransportAttempt::Unavailable(reason) => unavailable.push(format!("iroh: {reason}")),
        }
    }
    bail!(
        "target {} has no compatible runtime artifact transfer transport for {operation_name}: {}",
        target.node,
        unavailable.join("; ")
    )
}

enum TransportAttempt {
    Reply(TransferReply),
    Unavailable(String),
}

async fn http_exchange(
    cloud: &Arc<CloudState>,
    admin: &str,
    operation: &str,
    token: &str,
    frame: Bytes,
) -> TransportAttempt {
    let url = format!("{}{TRANSFER_PATH}/{operation}", admin.trim_end_matches('/'));
    let mut response = match cloud
        .http
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .body(frame)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return TransportAttempt::Unavailable(error.to_string()),
    };
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return TransportAttempt::Unavailable("NO_HANDLER (HTTP 404)".to_string());
    }
    if response
        .content_length()
        .is_some_and(|length| length > wire::MAX_CONTROL_BYTES as u64)
    {
        return TransportAttempt::Unavailable("reply content-length exceeds bound".to_string());
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > wire::MAX_CONTROL_BYTES {
                    return TransportAttempt::Unavailable("reply body exceeds bound".to_string());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => return TransportAttempt::Unavailable(error.to_string()),
        }
    }
    decode_transport_reply(body, "HTTP")
}

async fn iroh_exchange(
    cloud: &Arc<CloudState>,
    peer_id: &str,
    address: &str,
    operation: &str,
    token: &str,
    frame: &[u8],
) -> TransportAttempt {
    let path = format!("{TRANSFER_PATH}/{operation}?tok={token}");
    let body = crate::gossip::request_to_with_response_cap(
        cloud,
        peer_id,
        address,
        hive_p2p::GOSSIP_POST,
        &path,
        frame,
        REQUEST_TIMEOUT_SECS,
        wire::MAX_CONTROL_BYTES,
    )
    .await;
    match body {
        Some(body) => decode_transport_reply(body, "iroh"),
        None => TransportAttempt::Unavailable("request timed out or failed".to_string()),
    }
}

fn decode_transport_reply(body: Vec<u8>, transport: &str) -> TransportAttempt {
    if body.is_empty() {
        return TransportAttempt::Unavailable(format!("NO_HANDLER (empty {transport} reply)"));
    }
    match wire::decode_reply(&body) {
        Ok(reply) => TransportAttempt::Reply(reply),
        Err(error) => TransportAttempt::Unavailable(format!("untyped reply: {error:#}")),
    }
}

fn validate_reply(
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
    reply: TransferReply,
) -> anyhow::Result<TransferReply> {
    if reply.transaction_id != key.transaction_id
        || reply.generation_sha256 != key.generation_sha256
    {
        bail!(
            "target {} returned a runtime artifact reply for another transaction or generation",
            target.node
        );
    }
    if reply.next_offset > package.package_bytes {
        bail!(
            "target {} returned runtime artifact offset {} beyond package size {}",
            target.node,
            reply.next_offset,
            package.package_bytes
        );
    }
    if reply.code == ReplyCode::Ok {
        if reply.package_bytes != package.package_bytes || reply.participant_boot_nonce.is_empty() {
            bail!(
                "target {} returned an incomplete runtime artifact acknowledgement",
                target.node
            );
        }
        if materialized(reply.state) {
            if reply.next_offset != package.package_bytes
                || reply.semantic_tree_sha256 != package.semantic_tree_sha256
            {
                bail!(
                    "target {} materialized a different runtime artifact identity",
                    target.node
                );
            }
        } else if !reply.semantic_tree_sha256.is_empty() {
            bail!(
                "target {} reported semantic authority before materialization",
                target.node
            );
        }
    }
    Ok(reply)
}

fn receipt(
    target: &Target,
    key: &TransferKey,
    package: &hive_core::RuntimeArtifactPackageDescriptor,
    reply: TransferReply,
) -> anyhow::Result<TargetMaterializationReceipt> {
    if reply.code != ReplyCode::Ok || !materialized(reply.state) {
        bail!(
            "target {} has no materialized runtime artifact receipt",
            target.node
        );
    }
    Ok(TargetMaterializationReceipt {
        target_node: target.node.clone(),
        transaction_id: key.transaction_id.clone(),
        generation_sha256: key.generation_sha256.clone(),
        participant_boot_nonce: reply.participant_boot_nonce,
        package_sha256: package.package_sha256.clone(),
        semantic_tree_sha256: reply.semantic_tree_sha256,
        state: reply.state,
    })
}

fn refused<T>(target: &Target, operation: &str, reply: TransferReply) -> anyhow::Result<T> {
    bail!(
        "target {} refused runtime artifact {operation} with {:?} in state {:?}: {}",
        target.node,
        reply.code,
        reply.state,
        reply.message
    )
}

fn materialized(state: TransferState) -> bool {
    matches!(
        state,
        TransferState::Materialized | TransferState::Prepared | TransferState::Committed
    )
}

fn service_token(cloud: &Arc<CloudState>, key: &TransferKey) -> anyhow::Result<String> {
    if key.coordinator_node != cloud.node_name {
        bail!("runtime artifact transfer token coordinator does not match local node");
    }
    crate::auth::issue(
        &format!("mesh-node:{}", key.coordinator_node),
        &key.tenant,
        "service",
        false,
        SERVICE_TOKEN_TTL_SECS,
    )
    .context("mint runtime artifact transfer service token")
}

async fn read_package_at(
    file: Arc<std::fs::File>,
    offset: u64,
    length: usize,
) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let mut output = vec![0_u8; length];
        let mut filled = 0usize;
        while filled < output.len() {
            let position = offset
                .checked_add(filled as u64)
                .context("runtime artifact package read offset overflow")?;
            let read = file
                .read_at(&mut output[filled..], position)
                .context("read exact runtime artifact package range")?;
            if read == 0 {
                bail!("runtime artifact package ended before its verified descriptor");
            }
            filled += read;
        }
        Ok(output)
    })
    .await
    .context("join runtime artifact package read")?
}

fn target_set_sha256(targets: &[Target]) -> anyhow::Result<String> {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(
        &u32::try_from(targets.len())
            .context("runtime artifact target count overflow")?
            .to_be_bytes(),
    );
    for target in targets {
        let length = u16::try_from(target.node.len())
            .context("runtime artifact target node length overflow")?;
        canonical.extend_from_slice(&length.to_be_bytes());
        canonical.extend_from_slice(target.node.as_bytes());
    }
    let mut hash = Sha256::new();
    hash.update(TARGET_SET_DOMAIN);
    hash.update(canonical);
    Ok(format!("{:x}", hash.finalize()))
}

fn target_concurrency() -> anyhow::Result<usize> {
    let raw = match std::env::var("HIVE_ARTIFACT_TRANSFER_TARGET_CONCURRENCY") {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(DEFAULT_TARGET_CONCURRENCY),
        Err(error) => return Err(error).context("read artifact transfer target concurrency"),
    };
    raw.parse::<usize>()
        .ok()
        .filter(|value| (1..=MAX_TARGET_CONCURRENCY).contains(value))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "HIVE_ARTIFACT_TRANSFER_TARGET_CONCURRENCY must be 1..={MAX_TARGET_CONCURRENCY}"
            )
        })
}

fn operation_name(operation: Operation) -> &'static str {
    match operation {
        Operation::Begin => "begin",
        Operation::Chunk => "chunk",
        Operation::Query => "query",
        Operation::Finalize => "finalize",
        Operation::Abort => "abort",
        Operation::Prepare => "prepare",
        Operation::Commit => "commit",
        Operation::Reply => "reply",
    }
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

fn validate_text(value: &str, max: usize, label: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        bail!("{label} must contain 1..={max} bytes without control characters");
    }
    Ok(())
}

async fn retry_delay(attempt: usize) {
    let millis = 100_u64.saturating_mul(1_u64 << attempt.min(4));
    tokio::time::sleep(Duration::from_millis(millis)).await;
}
