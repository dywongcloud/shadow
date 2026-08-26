use crate::{BuildContractError, MetadataInputSeal, RepositoryBuildSnapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const DEPLOYMENT_BUILD_SNAPSHOT_SCHEMA: u16 = 2;
const SNAPSHOT_DOMAIN: &[u8] = b"hive-deployment-build-v2\0";
const MAX_IDENTITY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentSeal {
    pub sha256: String,
    pub bytes: u64,
}

impl ContentSeal {
    pub fn new(sha256: impl Into<String>, bytes: u64) -> Result<Self, BuildContractError> {
        let seal = Self {
            sha256: sha256.into(),
            bytes,
        };
        validate_sha256(&seal.sha256, "content seal")?;
        Ok(seal)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedOciIdentity {
    pub platform: String,
    pub manifest_digest: String,
    pub config_digest: String,
}

impl ResolvedOciIdentity {
    fn validate(&self) -> Result<(), BuildContractError> {
        validate_text(&self.platform, "OCI platform")?;
        validate_oci_digest(&self.manifest_digest, "OCI manifest digest")?;
        validate_oci_digest(&self.config_digest, "OCI config digest")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum SourceSnapshot {
    Git {
        repository: String,
        branch: Option<String>,
        commit: String,
        tree: String,
    },
    Zip {
        archive: ContentSeal,
    },
    PrebuiltOci {
        requested_ref: String,
        resolved: ResolvedOciIdentity,
    },
}

impl SourceSnapshot {
    fn validate(&self) -> Result<(), BuildContractError> {
        match self {
            Self::Git {
                repository,
                branch,
                commit,
                tree,
            } => {
                validate_text(repository, "Git repository")?;
                if let Some(branch) = branch {
                    validate_text(branch, "Git branch")?;
                }
                validate_git_object(commit, "Git commit")?;
                validate_git_object(tree, "Git tree")
            }
            Self::Zip { archive } => validate_sha256(&archive.sha256, "ZIP archive"),
            Self::PrebuiltOci {
                requested_ref,
                resolved,
            } => {
                validate_text(requested_ref, "requested OCI reference")?;
                resolved.validate()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildOutputInventorySeals {
    pub config: ContentSeal,
    pub functions: ContentSeal,
    pub static_files: ContentSeal,
}

impl BuildOutputInventorySeals {
    fn validate(&self) -> Result<(), BuildContractError> {
        validate_sha256(&self.config.sha256, "Build Output config inventory")?;
        validate_sha256(&self.functions.sha256, "Build Output function inventory")?;
        validate_sha256(&self.static_files.sha256, "Build Output static inventory")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeServiceIdentity {
    pub service: String,
    pub image: ResolvedOciIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum BuildAuthoritySnapshot {
    DetectedApplication {
        repository_contract: RepositoryBuildSnapshot,
        normalized_manifest_sha256: String,
    },
    ExplicitManifest {
        fluid_json: MetadataInputSeal,
        normalized_manifest_sha256: String,
    },
    BuildOutputV3 {
        inventory: BuildOutputInventorySeals,
        normalized_descriptor_sha256: String,
        normalized_manifest_sha256: String,
    },
    Dockerfile {
        dockerfile: MetadataInputSeal,
        context: ContentSeal,
        platform: String,
        normalized_build_inputs_sha256: String,
        resolved_image: ResolvedOciIdentity,
        normalized_manifest_sha256: String,
    },
    Compose {
        compose: MetadataInputSeal,
        normalized_service_topology_sha256: String,
        services: Vec<ComposeServiceIdentity>,
        normalized_manifest_sha256: String,
    },
    PrebuiltOci {
        resolved_image: ResolvedOciIdentity,
        normalized_manifest_sha256: String,
    },
}

impl BuildAuthoritySnapshot {
    fn normalize(&mut self) {
        if let Self::Compose { services, .. } = self {
            services.sort_by(|left, right| left.service.cmp(&right.service));
        }
    }

    fn validate(&self) -> Result<(), BuildContractError> {
        let manifest = match self {
            Self::DetectedApplication {
                repository_contract,
                normalized_manifest_sha256,
            } => {
                repository_contract.verify()?;
                normalized_manifest_sha256
            }
            Self::ExplicitManifest {
                fluid_json,
                normalized_manifest_sha256,
            } => {
                validate_metadata(fluid_json, "fluid.json")?;
                normalized_manifest_sha256
            }
            Self::BuildOutputV3 {
                inventory,
                normalized_descriptor_sha256,
                normalized_manifest_sha256,
            } => {
                inventory.validate()?;
                validate_sha256(normalized_descriptor_sha256, "Build Output descriptor")?;
                normalized_manifest_sha256
            }
            Self::Dockerfile {
                dockerfile,
                context,
                platform,
                normalized_build_inputs_sha256,
                resolved_image,
                normalized_manifest_sha256,
            } => {
                validate_metadata(dockerfile, "Dockerfile")?;
                validate_sha256(&context.sha256, "Docker build context")?;
                validate_text(platform, "Docker target platform")?;
                validate_sha256(normalized_build_inputs_sha256, "Docker build inputs")?;
                resolved_image.validate()?;
                if resolved_image.platform != *platform {
                    return Err(invalid(
                        "Docker resolved image platform differs from build platform",
                    ));
                }
                normalized_manifest_sha256
            }
            Self::Compose {
                compose,
                normalized_service_topology_sha256,
                services,
                normalized_manifest_sha256,
            } => {
                validate_metadata(compose, "Compose file")?;
                validate_sha256(
                    normalized_service_topology_sha256,
                    "Compose service topology",
                )?;
                if services.is_empty() {
                    return Err(invalid("Compose authority has no resolved services"));
                }
                let mut names = BTreeSet::new();
                for service in services {
                    validate_text(&service.service, "Compose service name")?;
                    if !names.insert(service.service.as_str()) {
                        return Err(invalid(
                            "Compose authority contains duplicate service names",
                        ));
                    }
                    service.image.validate()?;
                }
                normalized_manifest_sha256
            }
            Self::PrebuiltOci {
                resolved_image,
                normalized_manifest_sha256,
            } => {
                resolved_image.validate()?;
                normalized_manifest_sha256
            }
        };
        validate_sha256(manifest, "normalized deployment manifest")
    }

    pub fn normalized_manifest_sha256(&self) -> &str {
        match self {
            Self::DetectedApplication {
                normalized_manifest_sha256,
                ..
            }
            | Self::ExplicitManifest {
                normalized_manifest_sha256,
                ..
            }
            | Self::BuildOutputV3 {
                normalized_manifest_sha256,
                ..
            }
            | Self::Dockerfile {
                normalized_manifest_sha256,
                ..
            }
            | Self::Compose {
                normalized_manifest_sha256,
                ..
            }
            | Self::PrebuiltOci {
                normalized_manifest_sha256,
                ..
            } => normalized_manifest_sha256,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentBuildContract {
    pub schema: u16,
    pub source: SourceSnapshot,
    pub authority: BuildAuthoritySnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentBuildSnapshot {
    digest: String,
    contract: DeploymentBuildContract,
}

impl DeploymentBuildSnapshot {
    pub fn new(mut contract: DeploymentBuildContract) -> Result<Self, BuildContractError> {
        contract.schema = DEPLOYMENT_BUILD_SNAPSHOT_SCHEMA;
        contract.source.validate()?;
        contract.authority.normalize();
        contract.authority.validate()?;
        if let (
            SourceSnapshot::PrebuiltOci { resolved, .. },
            BuildAuthoritySnapshot::PrebuiltOci { resolved_image, .. },
        ) = (&contract.source, &contract.authority)
        {
            if resolved != resolved_image {
                return Err(invalid(
                    "prebuilt OCI source and build authority resolve to different identities",
                ));
            }
        } else if matches!(contract.source, SourceSnapshot::PrebuiltOci { .. }) {
            return Err(invalid(
                "prebuilt OCI source requires prebuilt OCI build authority",
            ));
        }
        let encoded = encode_contract(&contract)?;
        let mut digest = Sha256::new();
        digest.update(SNAPSHOT_DOMAIN);
        digest.update(&encoded);
        Ok(Self {
            digest: format!("{:x}", digest.finalize()),
            contract,
        })
    }

    pub fn verify(&self) -> Result<(), BuildContractError> {
        let rebuilt = Self::new(self.contract.clone())?;
        if self.digest != rebuilt.digest
            || encode_contract(&self.contract)? != encode_contract(&rebuilt.contract)?
        {
            return Err(invalid(
                "deployment build snapshot digest or canonical ordering does not match its contract",
            ));
        }
        Ok(())
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn contract(&self) -> &DeploymentBuildContract {
        &self.contract
    }

    pub fn canonical_contract_bytes(&self) -> Result<Vec<u8>, BuildContractError> {
        self.verify()?;
        encode_contract(&self.contract)
    }
}

fn encode_contract(contract: &DeploymentBuildContract) -> Result<Vec<u8>, BuildContractError> {
    serde_json::to_vec(contract).map_err(|error| {
        BuildContractError::invalid_metadata("encode deployment build snapshot", error)
    })
}

fn validate_metadata(input: &MetadataInputSeal, label: &str) -> Result<(), BuildContractError> {
    validate_sha256(&input.sha256, label)?;
    if input.path.as_str().is_empty() {
        return Err(invalid(format!("{label} metadata path is empty")));
    }
    Ok(())
}

fn validate_git_object(value: &str, label: &str) -> Result<(), BuildContractError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!(
            "{label} must be a 40- or 64-character lowercase hexadecimal object id"
        )));
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<(), BuildContractError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!(
            "{label} must be a 64-character lowercase hexadecimal SHA-256"
        )));
    }
    Ok(())
}

fn validate_oci_digest(value: &str, label: &str) -> Result<(), BuildContractError> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(invalid(format!("{label} must use sha256")));
    };
    validate_sha256(digest, label)
}

fn validate_text(value: &str, label: &str) -> Result<(), BuildContractError> {
    if value.is_empty() || value.len() > MAX_IDENTITY_BYTES || value.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{label} must be 1..={MAX_IDENTITY_BYTES} bytes with no control characters"
        )));
    }
    Ok(())
}

fn invalid(detail: impl Into<String>) -> BuildContractError {
    BuildContractError::invalid_metadata("seal deployment build snapshot", detail.into())
}
