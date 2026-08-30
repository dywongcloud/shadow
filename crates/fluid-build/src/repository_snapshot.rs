use crate::{
    BuildContractError, BuildContractErrorCode, OutputDirectory, PackageManagerDeclaration,
    PackageManagerDetection, PackageManagerLockfile, PackageManagerSource,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Component, Path};

const SNAPSHOT_SCHEMA: u16 = 1;
const SNAPSHOT_DOMAIN: &[u8] = b"hive-repository-build-v1\0";
const MAX_PATH_BYTES: usize = 4096;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_ARG_BYTES: usize = 4096;
const MAX_ARGS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryPath(String);

impl RepositoryPath {
    pub fn root() -> Self {
        Self(".".to_string())
    }

    pub fn parse(path: &Path) -> Result<Self, BuildContractError> {
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => {
                    let value = value.to_str().ok_or_else(|| {
                        BuildContractError::invalid_metadata(
                            "normalize repository path",
                            "repository paths must be valid UTF-8",
                        )
                    })?;
                    if value.chars().any(char::is_control) {
                        return Err(BuildContractError::invalid_metadata(
                            "normalize repository path",
                            "repository paths may not contain control characters",
                        ));
                    }
                    components.push(value);
                }
                Component::CurDir => {}
                _ => {
                    return Err(BuildContractError::invalid_metadata(
                        "normalize repository path",
                        "repository paths must be normalized and checkout-relative",
                    ));
                }
            }
        }
        let value = if components.is_empty() {
            ".".to_string()
        } else {
            components.join("/")
        };
        if value.len() > MAX_PATH_BYTES {
            return Err(BuildContractError::invalid_metadata(
                "normalize repository path",
                format!("repository path exceeds {MAX_PATH_BYTES} bytes"),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParentPath(String);

impl ParentPath {
    pub fn from_depth(depth: usize) -> Result<Self, BuildContractError> {
        if depth > 64 {
            return Err(BuildContractError::invalid_metadata(
                "derive repository path from selected application",
                "selected application exceeds 64 path components",
            ));
        }
        Ok(Self(if depth == 0 {
            ".".to_string()
        } else {
            std::iter::repeat_n("..", depth)
                .collect::<Vec<_>>()
                .join("/")
        }))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExactShellCommand(String);

impl ExactShellCommand {
    pub fn parse(value: String) -> Result<Self, BuildContractError> {
        if value.is_empty() || value.len() > MAX_COMMAND_BYTES || value.contains('\0') {
            return Err(BuildContractError::new(
                BuildContractErrorCode::InvalidMetadata,
                "validate explicit build command",
                format!(
                    "explicit command must be 1..={MAX_COMMAND_BYTES} bytes and contain no NUL"
                ),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixedArgv(Vec<String>);

impl FixedArgv {
    pub fn parse(values: Vec<String>) -> Result<Self, BuildContractError> {
        if values.is_empty()
            || values.len() > MAX_ARGS
            || values.iter().any(|value| {
                value.is_empty() || value.len() > MAX_ARG_BYTES || value.contains('\0')
            })
        {
            return Err(BuildContractError::invalid_metadata(
                "validate generated build command",
                format!(
                    "generated argv must contain 1..={MAX_ARGS} nonempty arguments of at most {MAX_ARG_BYTES} bytes"
                ),
            ));
        }
        Ok(Self(values))
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GeneratedStep {
    Install { use_npm_ci: bool },
    RunBuildScript,
    FrameworkExec { argv: FixedArgv },
    TurboBuild { repository_from_app: ParentPath },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "authority", content = "value")]
pub enum StepAuthority {
    None,
    Explicit(ExactShellCommand),
    Generated(GeneratedStep),
}

impl StepAuthority {
    pub fn explicit(value: Option<String>) -> Result<Option<Self>, BuildContractError> {
        value
            .map(ExactShellCommand::parse)
            .transpose()
            .map(|value| value.map(Self::Explicit))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageManagerSnapshot {
    pub manager: String,
    pub source: PackageManagerSource,
    pub declaration: Option<PackageManagerDeclaration>,
    pub lockfile: Option<PackageManagerLockfile>,
    pub diagnostics: Vec<String>,
}

impl From<&PackageManagerDetection> for PackageManagerSnapshot {
    fn from(value: &PackageManagerDetection) -> Self {
        let mut diagnostics = value.conflict_warning.iter().cloned().collect::<Vec<_>>();
        diagnostics.extend(value.validation_error.iter().cloned());
        diagnostics.sort();
        diagnostics.dedup();
        Self {
            manager: value.manager.to_string(),
            source: value.source,
            declaration: value.declaration.clone(),
            lockfile: value.lockfile,
            diagnostics,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub source: String,
    pub members: Vec<RepositoryPath>,
}

impl WorkspaceSnapshot {
    pub fn new(
        source: impl Into<String>,
        members: &[std::path::PathBuf],
    ) -> Result<Self, BuildContractError> {
        let mut normalized = members
            .iter()
            .map(|member| RepositoryPath::parse(member))
            .collect::<Result<Vec<_>, _>>()?;
        normalized.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        normalized.dedup();
        Ok(Self {
            source: source.into(),
            members: normalized,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApplicationSnapshot {
    pub selected: RepositoryPath,
    pub source: String,
    pub evidence: Vec<String>,
    pub decision_digest: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameworkSnapshot {
    pub slug: String,
    pub name: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildSteps {
    pub install: StepAuthority,
    pub build: StepAuthority,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepositoryCoordinates {
    pub checkout_root: RepositoryPath,
    pub install_root: RepositoryPath,
    pub selected_app: RepositoryPath,
    pub build_cwd: RepositoryPath,
    pub output_root: RepositoryPath,
    pub runtime_artifact_base: RepositoryPath,
    pub function_cwd: RepositoryPath,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetadataInputSeal {
    pub path: RepositoryPath,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepositoryBuildContract {
    pub schema: u16,
    pub package_manager: PackageManagerSnapshot,
    pub workspace: Option<WorkspaceSnapshot>,
    pub application: ApplicationSnapshot,
    pub framework: FrameworkSnapshot,
    pub steps: BuildSteps,
    pub output: OutputDirectory,
    pub coordinates: RepositoryCoordinates,
    pub metadata: Vec<MetadataInputSeal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepositoryBuildSnapshot {
    digest: String,
    contract: RepositoryBuildContract,
}

impl RepositoryBuildSnapshot {
    pub fn new(mut contract: RepositoryBuildContract) -> Result<Self, BuildContractError> {
        contract.schema = SNAPSHOT_SCHEMA;
        contract.application.evidence.sort();
        contract.application.evidence.dedup();
        contract
            .metadata
            .sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
        let mut seen = BTreeSet::new();
        if contract
            .metadata
            .iter()
            .any(|input| !seen.insert(input.path.as_str()))
        {
            return Err(BuildContractError::invalid_metadata(
                "seal repository build metadata",
                "repository metadata inputs contain duplicate paths",
            ));
        }
        let encoded = serde_json::to_vec(&contract).map_err(|error| {
            BuildContractError::invalid_metadata("encode repository build snapshot", error)
        })?;
        let mut digest = Sha256::new();
        digest.update(SNAPSHOT_DOMAIN);
        digest.update(encoded);
        let digest = format!("{:x}", digest.finalize());
        Ok(Self { digest, contract })
    }

    pub fn verify(&self) -> Result<(), BuildContractError> {
        let rebuilt = Self::new(self.contract.clone())?;
        let current = serde_json::to_vec(&self.contract).map_err(|error| {
            BuildContractError::invalid_metadata("verify repository build snapshot", error)
        })?;
        let canonical = serde_json::to_vec(&rebuilt.contract).map_err(|error| {
            BuildContractError::invalid_metadata("verify repository build snapshot", error)
        })?;
        if rebuilt.digest != self.digest || current != canonical {
            return Err(BuildContractError::invalid_metadata(
                "verify repository build snapshot",
                "repository build snapshot digest or canonical ordering does not match its contract",
            ));
        }
        Ok(())
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn contract(&self) -> &RepositoryBuildContract {
        &self.contract
    }
}
