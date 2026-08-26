use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SENTINEL: &str = "HIVE_DEPLOYMENT_LEDGER_V1";
const SCHEMA: u16 = 1;
const CHECKSUM_DOMAIN: &[u8] = b"hive-deployment-ledger-v1\0";
const MAX_LEDGER_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Git,
    Upload,
    PrebuiltImage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub kind: SourceKind,
    pub repository: String,
    pub branch: String,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeploymentAcceptanceInput {
    pub deployment_id: String,
    pub project: String,
    pub target: String,
    pub source: SourceIdentity,
    pub repository_build: Option<fluid_build::RepositoryBuildSnapshot>,
    pub runtime_artifact: Option<hive_core::RuntimeArtifactIdentity>,
    pub readiness: fluid_gateway::DeploymentReadinessReceipt,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeploymentAcceptance {
    #[serde(flatten)]
    pub input: DeploymentAcceptanceInput,
    pub accepting_node: String,
    pub accepting_boot_nonce: String,
    pub accepted_ms: u64,
    pub published_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyReadyRecord {
    pub deployment_id: String,
    pub project: String,
    pub deployment_created_ms: u64,
    pub imported_ms: u64,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreAuthority {
    Proven,
    LegacyMigration,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasTransactionState {
    Prepared,
    Applied,
    Aborted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AliasTransaction {
    pub revision: u64,
    pub project: String,
    pub from_deployment: Option<String>,
    pub to_deployment: String,
    pub prepared_ms: u64,
    pub state: AliasTransactionState,
    pub finished_ms: Option<u64>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LifecycleOutboxEntry {
    pub id: String,
    pub event: String,
    pub project: String,
    pub deployment_id: String,
    pub payload: Value,
    pub created_ms: u64,
    pub delivered_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LedgerPayload {
    schema: u16,
    node: String,
    created_ms: u64,
    boot_nonce: String,
    revision: u64,
    alias_revision: u64,
    accepted: BTreeMap<String, DeploymentAcceptance>,
    legacy_ready: BTreeMap<String, LegacyReadyRecord>,
    aliases: Vec<AliasTransaction>,
    outbox: BTreeMap<String, LifecycleOutboxEntry>,
}

#[derive(Serialize, Deserialize)]
struct LedgerFile {
    sentinel: String,
    payload: LedgerPayload,
    checksum_sha256: String,
}

pub struct DeploymentLedger {
    path: PathBuf,
    state: Mutex<LedgerPayload>,
}

impl DeploymentLedger {
    pub fn open(path: PathBuf, node: &str) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            !node.trim().is_empty(),
            "deployment ledger node identity is empty"
        );
        let boot_nonce = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut payload = match load_payload(&path)? {
            Some(payload) => {
                anyhow::ensure!(
                    payload.node == node,
                    "deployment ledger belongs to node {:?}, not {:?}",
                    payload.node,
                    node
                );
                payload
            }
            None => LedgerPayload {
                schema: SCHEMA,
                node: node.to_string(),
                created_ms: hive_core::now_ms(),
                boot_nonce: String::new(),
                revision: 0,
                alias_revision: 0,
                accepted: BTreeMap::new(),
                legacy_ready: BTreeMap::new(),
                aliases: Vec::new(),
                outbox: BTreeMap::new(),
            },
        };
        validate_payload(&payload)?;
        payload.boot_nonce = boot_nonce;
        payload.revision = payload
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("deployment ledger revision overflow"))?;
        write_payload(&path, &payload)?;
        Ok(Arc::new(Self {
            path,
            state: Mutex::new(payload),
        }))
    }

    pub fn boot_nonce(&self) -> String {
        self.state.lock().boot_nonce.clone()
    }

    pub fn authorize_restore_batch(
        &self,
        deployments: &[(String, String, u64)],
    ) -> anyhow::Result<std::collections::HashMap<String, RestoreAuthority>> {
        self.mutate(|payload| {
            let mut authorities = std::collections::HashMap::new();
            for (deployment_id, project, deployment_created_ms) in deployments {
                validate_identifier(deployment_id, "restored deployment id")?;
                validate_identifier(project, "restored deployment project")?;
                anyhow::ensure!(
                    authorities.insert(deployment_id.clone(), RestoreAuthority::Refused).is_none(),
                    "restore authority input contains duplicate deployment {deployment_id}"
                );
                if let Some(accepted) = payload.accepted.get(deployment_id) {
                    if accepted.input.project == *project && accepted.published_ms.is_some() {
                        authorities.insert(deployment_id.clone(), RestoreAuthority::Proven);
                    }
                    continue;
                }
                if let Some(legacy) = payload.legacy_ready.get(deployment_id) {
                    if legacy.project == *project
                        && legacy.deployment_created_ms == *deployment_created_ms
                    {
                        authorities
                            .insert(deployment_id.clone(), RestoreAuthority::LegacyMigration);
                    }
                    continue;
                }
                if *deployment_created_ms > payload.created_ms {
                    continue;
                }
                payload.legacy_ready.insert(
                    deployment_id.clone(),
                    LegacyReadyRecord {
                        deployment_id: deployment_id.clone(),
                        project: project.clone(),
                        deployment_created_ms: *deployment_created_ms,
                        imported_ms: hive_core::now_ms(),
                        reason: "ready record predates deployment-ledger installation; preserved only as a serving predecessor and never accepted as readiness proof"
                            .to_string(),
                    },
                );
                authorities.insert(deployment_id.clone(), RestoreAuthority::LegacyMigration);
            }
            Ok(authorities)
        })
    }

    pub fn has_proven_acceptance(&self, deployment_id: &str) -> bool {
        self.state
            .lock()
            .accepted
            .get(deployment_id)
            .is_some_and(|acceptance| acceptance.published_ms.is_some())
    }

    pub fn accept(&self, input: DeploymentAcceptanceInput) -> anyhow::Result<()> {
        validate_acceptance_input(&input)?;
        self.mutate(|payload| {
            let accepted = DeploymentAcceptance {
                input,
                accepting_node: payload.node.clone(),
                accepting_boot_nonce: payload.boot_nonce.clone(),
                accepted_ms: hive_core::now_ms(),
                published_ms: None,
            };
            if let Some(existing) = payload.accepted.get(&accepted.input.deployment_id) {
                anyhow::ensure!(
                    canonical_bytes(&existing.input)? == canonical_bytes(&accepted.input)?,
                    "deployment {} already has different acceptance evidence",
                    accepted.input.deployment_id
                );
                return Ok(());
            }
            payload
                .accepted
                .insert(accepted.input.deployment_id.clone(), accepted);
            Ok(())
        })
    }

    pub fn mark_published(
        &self,
        deployment_id: &str,
        event_payload: Value,
    ) -> anyhow::Result<String> {
        validate_identifier(deployment_id, "deployment id")?;
        self.mutate(|payload| {
            let accepted = payload.accepted.get_mut(deployment_id).ok_or_else(|| {
                anyhow::anyhow!("deployment {deployment_id} has no acceptance record")
            })?;
            let published_ms = match accepted.published_ms {
                Some(published_ms) => published_ms,
                None => {
                    let published_ms = hive_core::now_ms();
                    accepted.published_ms = Some(published_ms);
                    published_ms
                }
            };
            let id = format!("deployment.ready:{deployment_id}");
            let event = LifecycleOutboxEntry {
                id: id.clone(),
                event: "deployment.ready".to_string(),
                project: accepted.input.project.clone(),
                deployment_id: deployment_id.to_string(),
                payload: event_payload,
                created_ms: published_ms,
                delivered_ms: None,
            };
            insert_outbox(payload, event)?;
            Ok(id)
        })
    }

    pub fn prepare_alias(
        &self,
        project: &str,
        from_deployment: Option<String>,
        to_deployment: &str,
    ) -> anyhow::Result<u64> {
        validate_identifier(project, "project")?;
        validate_identifier(to_deployment, "target deployment id")?;
        if let Some(from) = from_deployment.as_deref() {
            validate_identifier(from, "prior deployment id")?;
        }
        self.mutate(|payload| {
            let accepted = payload.accepted.get(to_deployment).ok_or_else(|| {
                anyhow::anyhow!("deployment {to_deployment} has no acceptance record")
            })?;
            anyhow::ensure!(
                accepted.published_ms.is_some(),
                "deployment {to_deployment} was accepted but not durably published"
            );
            anyhow::ensure!(
                accepted.input.project == project,
                "deployment {to_deployment} belongs to project {:?}, not {:?}",
                accepted.input.project,
                project
            );
            if let Some(existing) = payload.aliases.iter().rev().find(|transaction| {
                transaction.project == project
                    && transaction.to_deployment == to_deployment
                    && transaction.state == AliasTransactionState::Prepared
            }) {
                anyhow::ensure!(
                    existing.from_deployment == from_deployment,
                    "prepared alias transaction {} has different predecessor evidence",
                    existing.revision
                );
                return Ok(existing.revision);
            }
            payload.alias_revision = payload
                .alias_revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("deployment alias revision overflow"))?;
            let revision = payload.alias_revision;
            payload.aliases.push(AliasTransaction {
                revision,
                project: project.to_string(),
                from_deployment,
                to_deployment: to_deployment.to_string(),
                prepared_ms: hive_core::now_ms(),
                state: AliasTransactionState::Prepared,
                finished_ms: None,
                reason: None,
            });
            Ok(revision)
        })
    }

    pub fn mark_alias_applied(
        &self,
        revision: u64,
        event_payload: Value,
    ) -> anyhow::Result<String> {
        self.mutate(|payload| {
            let transaction = payload
                .aliases
                .iter_mut()
                .find(|transaction| transaction.revision == revision)
                .ok_or_else(|| anyhow::anyhow!("unknown alias revision {revision}"))?;
            anyhow::ensure!(
                matches!(
                    transaction.state,
                    AliasTransactionState::Prepared | AliasTransactionState::Applied
                ),
                "alias revision {revision} is aborted"
            );
            if transaction.state == AliasTransactionState::Prepared {
                transaction.state = AliasTransactionState::Applied;
                transaction.finished_ms = Some(hive_core::now_ms());
            }
            let id = format!("deployment.promoted:{revision}");
            let event = LifecycleOutboxEntry {
                id: id.clone(),
                event: "deployment.promoted".to_string(),
                project: transaction.project.clone(),
                deployment_id: transaction.to_deployment.clone(),
                payload: event_payload,
                created_ms: transaction.finished_ms.unwrap_or(transaction.prepared_ms),
                delivered_ms: None,
            };
            insert_outbox(payload, event)?;
            Ok(id)
        })
    }

    pub fn abort_alias(&self, revision: u64, reason: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!reason.trim().is_empty(), "alias abort reason is empty");
        self.mutate(|payload| {
            let transaction = payload
                .aliases
                .iter_mut()
                .find(|transaction| transaction.revision == revision)
                .ok_or_else(|| anyhow::anyhow!("unknown alias revision {revision}"))?;
            anyhow::ensure!(
                transaction.state != AliasTransactionState::Applied,
                "applied alias revision {revision} cannot be aborted"
            );
            transaction.state = AliasTransactionState::Aborted;
            transaction.finished_ms = Some(hive_core::now_ms());
            transaction.reason = Some(reason.to_string());
            Ok(())
        })
    }

    pub fn pending_aliases(&self) -> Vec<AliasTransaction> {
        self.state
            .lock()
            .aliases
            .iter()
            .filter(|transaction| transaction.state == AliasTransactionState::Prepared)
            .cloned()
            .collect()
    }

    pub fn acceptance(&self, deployment_id: &str) -> Option<DeploymentAcceptance> {
        self.state.lock().accepted.get(deployment_id).cloned()
    }

    pub fn pending_outbox(&self) -> Vec<LifecycleOutboxEntry> {
        self.state
            .lock()
            .outbox
            .values()
            .filter(|entry| entry.delivered_ms.is_none())
            .cloned()
            .collect()
    }

    pub fn mark_event_delivered(&self, event_id: &str) -> anyhow::Result<()> {
        validate_identifier(event_id, "lifecycle event id")?;
        self.mutate(|payload| {
            let entry = payload
                .outbox
                .get_mut(event_id)
                .ok_or_else(|| anyhow::anyhow!("unknown lifecycle event {event_id:?}"))?;
            entry.delivered_ms.get_or_insert_with(hive_core::now_ms);
            Ok(())
        })
    }

    fn mutate<T>(
        &self,
        mutation: impl FnOnce(&mut LedgerPayload) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut current = self.state.lock();
        let mut next = current.clone();
        let result = mutation(&mut next)?;
        validate_payload(&next)?;
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("deployment ledger revision overflow"))?;
        write_payload(&self.path, &next)?;
        *current = next;
        Ok(result)
    }
}

pub fn spawn_outbox(cloud: Arc<crate::state::CloudState>) {
    tokio::spawn(async move {
        loop {
            drain_outbox_once(&cloud).await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

pub async fn drain_outbox_once(cloud: &Arc<crate::state::CloudState>) {
    for entry in cloud.deployment_ledger.pending_outbox() {
        let delivered = crate::webhooks::dispatch_durable(
            &cloud.webhooks,
            &entry.id,
            entry.created_ms,
            &entry.project,
            &entry.event,
            entry.payload,
        )
        .await;
        if !delivered {
            tracing::warn!(
                event_id = %entry.id,
                event = %entry.event,
                "deployment lifecycle event remains pending after webhook refusal"
            );
            continue;
        }
        if let Err(error) = cloud.deployment_ledger.mark_event_delivered(&entry.id) {
            tracing::error!(
                event_id = %entry.id,
                %error,
                "webhook delivery succeeded but lifecycle outbox acknowledgement failed"
            );
        }
    }
}

fn insert_outbox(payload: &mut LedgerPayload, event: LifecycleOutboxEntry) -> anyhow::Result<()> {
    if let Some(existing) = payload.outbox.get(&event.id) {
        anyhow::ensure!(
            existing.id == event.id
                && existing.event == event.event
                && existing.project == event.project
                && existing.deployment_id == event.deployment_id
                && canonical_bytes(&existing.payload)? == canonical_bytes(&event.payload)?
                && existing.created_ms == event.created_ms,
            "lifecycle event {:?} already has different durable content",
            event.id
        );
        return Ok(());
    }
    payload.outbox.insert(event.id.clone(), event);
    Ok(())
}

fn validate_acceptance_input(input: &DeploymentAcceptanceInput) -> anyhow::Result<()> {
    validate_identifier(&input.deployment_id, "deployment id")?;
    validate_identifier(&input.project, "project")?;
    anyhow::ensure!(
        matches!(input.target.as_str(), "production" | "preview"),
        "deployment target must be production or preview"
    );
    anyhow::ensure!(
        input.readiness.deployment_id == input.deployment_id,
        "readiness receipt belongs to deployment {:?}, not {:?}",
        input.readiness.deployment_id,
        input.deployment_id
    );
    validate_source(&input.source)?;
    if let Some(snapshot) = input.repository_build.as_ref() {
        snapshot.verify()?;
    }
    if let Some(artifact) = input.runtime_artifact.as_ref() {
        anyhow::ensure!(
            artifact.protocol == hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION,
            "runtime artifact protocol {} is not supported",
            artifact.protocol
        );
        validate_identifier(&artifact.id, "runtime artifact id")?;
        validate_digest(&artifact.content_sha256, "runtime artifact content digest")?;
    }
    let mut previous = None;
    for function in &input.readiness.functions {
        validate_identifier(&function.function, "readiness function")?;
        validate_identifier(&function.cell_id, "readiness cell id")?;
        anyhow::ensure!(
            previous
                .as_deref()
                .is_none_or(|name| name < function.function.as_str()),
            "readiness functions are not unique and lexically ordered"
        );
        anyhow::ensure!(
            function.raw_bound || function.status.is_some(),
            "readiness function {:?} has neither protocol proof nor HTTP status",
            function.function
        );
        previous = Some(function.function.clone());
    }
    Ok(())
}

fn validate_source(source: &SourceIdentity) -> anyhow::Result<()> {
    anyhow::ensure!(
        !source.revision.trim().is_empty(),
        "deployment source revision is empty"
    );
    anyhow::ensure!(
        source.revision.len() <= 4096 && !source.revision.contains('\0'),
        "deployment source revision is invalid"
    );
    for (name, value) in [
        ("source repository", source.repository.as_str()),
        ("source branch", source.branch.as_str()),
    ] {
        anyhow::ensure!(
            value.len() <= 16 * 1024 && !value.contains('\0'),
            "{name} is invalid"
        );
    }
    Ok(())
}

fn validate_payload(payload: &LedgerPayload) -> anyhow::Result<()> {
    anyhow::ensure!(
        payload.schema == SCHEMA,
        "unsupported deployment ledger schema"
    );
    anyhow::ensure!(
        !payload.node.trim().is_empty(),
        "deployment ledger node is empty"
    );
    anyhow::ensure!(
        payload.created_ms > 0,
        "deployment ledger creation time is missing"
    );
    if !payload.boot_nonce.is_empty() {
        validate_digest(&payload.boot_nonce, "deployment ledger boot nonce")?;
    }
    let mut last_revision = 0;
    for transaction in &payload.aliases {
        anyhow::ensure!(
            transaction.revision > last_revision && transaction.revision <= payload.alias_revision,
            "deployment alias revisions are not strictly ordered"
        );
        last_revision = transaction.revision;
    }
    for (deployment_id, acceptance) in &payload.accepted {
        anyhow::ensure!(
            deployment_id == &acceptance.input.deployment_id,
            "deployment acceptance key does not match its record"
        );
        validate_acceptance_input(&acceptance.input)?;
        anyhow::ensure!(
            acceptance.accepting_node == payload.node,
            "deployment acceptance was written for a different node"
        );
        validate_digest(
            &acceptance.accepting_boot_nonce,
            "deployment acceptance boot nonce",
        )?;
        anyhow::ensure!(
            !payload.legacy_ready.contains_key(deployment_id),
            "deployment {deployment_id} has both proven and legacy restore authority"
        );
    }
    for (deployment_id, legacy) in &payload.legacy_ready {
        anyhow::ensure!(
            deployment_id == &legacy.deployment_id,
            "legacy-ready key does not match its record"
        );
        validate_identifier(deployment_id, "legacy-ready deployment id")?;
        validate_identifier(&legacy.project, "legacy-ready project")?;
        anyhow::ensure!(
            legacy.deployment_created_ms <= payload.created_ms,
            "legacy-ready deployment {deployment_id} does not predate the ledger"
        );
        anyhow::ensure!(
            !legacy.reason.trim().is_empty(),
            "legacy-ready deployment {deployment_id} has no migration reason"
        );
    }
    for (event_id, event) in &payload.outbox {
        anyhow::ensure!(event_id == &event.id, "lifecycle outbox key mismatch");
        validate_identifier(&event.id, "lifecycle event id")?;
        validate_identifier(&event.project, "lifecycle event project")?;
        validate_identifier(&event.deployment_id, "lifecycle event deployment")?;
        anyhow::ensure!(
            matches!(
                event.event.as_str(),
                "deployment.ready" | "deployment.promoted"
            ),
            "unsupported lifecycle outbox event {:?}",
            event.event
        );
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control),
        "{label} is invalid"
    );
    Ok(())
}

fn validate_digest(value: &str, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} must be 64 lowercase hexadecimal characters"
    );
    Ok(())
}

fn canonical_bytes(value: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(value).context("serialize deployment ledger value")
}

fn payload_checksum(payload: &LedgerPayload) -> anyhow::Result<String> {
    let mut digest = Sha256::new();
    digest.update(CHECKSUM_DOMAIN);
    digest.update(canonical_bytes(payload)?);
    Ok(format!("{:x}", digest.finalize()))
}

fn load_payload(path: &Path) -> anyhow::Result<Option<LedgerPayload>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open deployment ledger"),
    };
    let size = file.metadata()?.len();
    anyhow::ensure!(
        size <= MAX_LEDGER_BYTES,
        "deployment ledger exceeds {MAX_LEDGER_BYTES} bytes"
    );
    let mut bytes = Vec::with_capacity(size as usize);
    BufReader::new(file)
        .take(MAX_LEDGER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_LEDGER_BYTES,
        "deployment ledger exceeds {MAX_LEDGER_BYTES} bytes"
    );
    let stored: LedgerFile = serde_json::from_slice(&bytes).context("decode deployment ledger")?;
    anyhow::ensure!(
        stored.sentinel == SENTINEL,
        "deployment ledger sentinel mismatch"
    );
    validate_digest(&stored.checksum_sha256, "deployment ledger checksum")?;
    anyhow::ensure!(
        payload_checksum(&stored.payload)? == stored.checksum_sha256,
        "deployment ledger checksum mismatch"
    );
    validate_payload(&stored.payload)?;
    Ok(Some(stored.payload))
}

fn write_payload(path: &Path, payload: &LedgerPayload) -> anyhow::Result<()> {
    validate_payload(payload)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("deployment ledger path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let checksum_sha256 = payload_checksum(payload)?;
    let file = LedgerFile {
        sentinel: SENTINEL.to_string(),
        payload: payload.clone(),
        checksum_sha256,
    };
    let bytes = canonical_bytes(&file)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_LEDGER_BYTES,
        "deployment ledger would exceed {MAX_LEDGER_BYTES} bytes"
    );
    let temp = parent.join(format!(".deployment-ledger-v1.{}.tmp", payload.boot_nonce));
    let write_result = (|| -> anyhow::Result<()> {
        let output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .context("create deployment ledger temporary file")?;
        {
            let mut writer = BufWriter::new(&output);
            writer.write_all(&bytes)?;
            writer.flush()?;
        }
        output.sync_all()?;
        std::fs::rename(&temp, path).context("atomically publish deployment ledger")?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    write_result
}
