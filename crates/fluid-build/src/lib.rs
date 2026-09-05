//! `fluid-build` — Framework-Defined Infrastructure.
//!
//! Turns a source repository into the **Build Output API v3** contract: detect
//! the framework, run its build, and normalize the result into static assets +
//! serverless/edge functions + routing that the platform provisions from.
//!
//! Two paths:
//! 1. **Native** — the framework (Next.js, Nuxt, SvelteKit, Remix…) emits
//!    `.vercel/output` directly (via its `vercel build` adapter). We
//!    [`parse_build_output`] it.
//! 2. **Adapted** — for frameworks without an adapter (Vite, CRA, Astro, plain
//!    static, a Node server) we run the build and *synthesize* a Build Output
//!    from the native output directory.

pub mod build_output;
pub mod deployment_snapshot;
pub mod framework;
pub mod nextjs;
pub mod parser;
pub mod per_route;
pub mod repository_snapshot;
pub mod vercel_config;

pub use build_output::{BuildOutputConfig, FunctionConfig, Route, BUILD_OUTPUT_VERSION};
pub use deployment_snapshot::{
    BuildAuthoritySnapshot, BuildOutputInventorySeals, ComposeServiceIdentity, ContentSeal,
    DeploymentBuildContract, DeploymentBuildSnapshot, ResolvedOciIdentity, SourceSnapshot,
    DEPLOYMENT_BUILD_SNAPSHOT_SCHEMA,
};
pub use framework::{
    detect, detect_checked, detect_package_manager, detect_package_manager_checked,
    package_manager, plan_build, plan_build_checked_with_package_manager,
    plan_build_with_package_manager, preset_by_name, BuildPlan, FrameworkPreset,
    PackageManagerDeclaration, PackageManagerDetection, PackageManagerLockfile,
    PackageManagerSource, Primitive, PRESETS,
};
pub use nextjs::{detect_features, BuildFeatures};
pub use parser::{has_build_output, parse_build_output, BuildOutput, DeployedFunction};
pub use repository_snapshot::{
    ApplicationSnapshot, BuildSteps, ExactShellCommand, FixedArgv, FrameworkSnapshot,
    GeneratedStep, MetadataInputSeal, PackageManagerSnapshot, ParentPath, RepositoryBuildContract,
    RepositoryBuildSnapshot, RepositoryCoordinates, RepositoryPath, StepAuthority,
    WorkspaceSnapshot,
};
pub use vercel_config::{
    load_vercel_config, load_vercel_config_checked, ConditionValue, VercelCondition, VercelConfig,
    VercelCron, VercelFunction, VercelHeader, VercelHeaderRule, VercelImages, VercelRedirect,
    VercelRewrite,
};

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

pub const BUILD_CONTRACT_INVALID_METADATA: &str = "BUILD_CONTRACT_INVALID_METADATA";
pub const BUILD_CONTRACT_INVALID_FRAMEWORK: &str = "BUILD_CONTRACT_INVALID_FRAMEWORK";
pub const BUILD_CONTRACT_INVALID_OUTPUT_DIRECTORY: &str = "BUILD_CONTRACT_INVALID_OUTPUT_DIRECTORY";
pub const BUILD_CONTRACT_INVALID_BUILD_OUTPUT: &str = "BUILD_CONTRACT_INVALID_BUILD_OUTPUT";
pub const BUILD_CONTRACT_UNSUPPORTED_BUILD_OUTPUT: &str = "BUILD_CONTRACT_UNSUPPORTED_BUILD_OUTPUT";
pub const BUILD_CONTRACT_INVALID_FORWARDED_SETTINGS: &str =
    "BUILD_CONTRACT_INVALID_FORWARDED_SETTINGS";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildContractErrorCode {
    InvalidMetadata,
    InvalidFramework,
    InvalidOutputDirectory,
    InvalidBuildOutput,
    UnsupportedBuildOutput,
    InvalidForwardedSettings,
}

impl BuildContractErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidMetadata => BUILD_CONTRACT_INVALID_METADATA,
            Self::InvalidFramework => BUILD_CONTRACT_INVALID_FRAMEWORK,
            Self::InvalidOutputDirectory => BUILD_CONTRACT_INVALID_OUTPUT_DIRECTORY,
            Self::InvalidBuildOutput => BUILD_CONTRACT_INVALID_BUILD_OUTPUT,
            Self::UnsupportedBuildOutput => BUILD_CONTRACT_UNSUPPORTED_BUILD_OUTPUT,
            Self::InvalidForwardedSettings => BUILD_CONTRACT_INVALID_FORWARDED_SETTINGS,
        }
    }
}

#[derive(Debug)]
pub struct BuildContractError {
    pub code: BuildContractErrorCode,
    pub operation: &'static str,
    detail: String,
}

impl BuildContractError {
    pub fn new(
        code: BuildContractErrorCode,
        operation: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            operation,
            detail: detail.into(),
        }
    }

    pub fn invalid_metadata(operation: &'static str, detail: impl fmt::Display) -> Self {
        Self::new(
            BuildContractErrorCode::InvalidMetadata,
            operation,
            detail.to_string(),
        )
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for BuildContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}: {}",
            self.code.as_str(),
            self.operation,
            self.detail
        )
    }
}

impl std::error::Error for BuildContractError {}

/// A normalized output directory relative to the selected application. This is
/// the only output-path form accepted by checked build planning.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputDirectory(String);

impl OutputDirectory {
    pub const MAX_BYTES: usize = 4096;

    pub fn root() -> Self {
        Self(".".to_string())
    }

    pub fn parse(value: &str) -> Result<Self, BuildContractError> {
        if value.len() > Self::MAX_BYTES || value.chars().any(char::is_control) {
            return Err(BuildContractError::new(
                BuildContractErrorCode::InvalidOutputDirectory,
                "normalize outputDirectory",
                format!(
                    "outputDirectory must be at most {} bytes and contain no control characters",
                    Self::MAX_BYTES
                ),
            ));
        }
        let value = value.trim().replace('\\', "/");
        let windows_absolute = value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic);
        if value.starts_with('/') || windows_absolute {
            return Err(BuildContractError::new(
                BuildContractErrorCode::InvalidOutputDirectory,
                "normalize outputDirectory",
                format!("outputDirectory {value:?} must be checkout-relative"),
            ));
        }
        let mut components = Vec::new();
        for component in value.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    return Err(BuildContractError::new(
                        BuildContractErrorCode::InvalidOutputDirectory,
                        "normalize outputDirectory",
                        format!("outputDirectory {value:?} may not traverse above the checkout"),
                    ));
                }
                component => components.push(component),
            }
        }
        let normalized = if components.is_empty() {
            ".".to_string()
        } else {
            components.join("/")
        };
        if normalized.len() > Self::MAX_BYTES {
            return Err(BuildContractError::new(
                BuildContractErrorCode::InvalidOutputDirectory,
                "normalize outputDirectory",
                format!(
                    "normalized outputDirectory exceeds the {}-byte limit",
                    Self::MAX_BYTES
                ),
            ));
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OutputDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub struct CheckedBuildResolution {
    pub plan: BuildPlan,
    /// Present when the selected application already carries Build Output API v3.
    pub build_output: Option<BuildOutput>,
}

/// A read-only analysis of a repo — what the dashboard shows on the "Configure
/// Project" screen before a build runs.
#[derive(Clone, Debug, Serialize)]
pub struct Analysis {
    pub framework_slug: String,
    pub framework_name: String,
    pub primitive: Primitive,
    /// Whether the repo already ships a Build Output (`.vercel/output`).
    pub has_build_output: bool,
    pub package_manager: String,
    pub package_manager_declaration: Option<PackageManagerDeclaration>,
    pub package_manager_error: Option<String>,
    pub install_command: String,
    pub build_command: String,
    pub output_dir: String,
}

/// Inspect a repo without building it.
pub fn analyze(repo: &Path) -> Analysis {
    let plan = plan_build(repo, None, None, None, None);
    Analysis {
        framework_slug: plan.framework.slug.to_string(),
        framework_name: plan.framework.name.to_string(),
        primitive: plan.framework.primitive,
        has_build_output: has_build_output(repo),
        package_manager: plan.package_manager,
        package_manager_declaration: plan.package_manager_declaration,
        package_manager_error: plan.package_manager_error,
        install_command: plan.install_command,
        build_command: plan.build_command,
        output_dir: plan.output_dir.as_str().to_string(),
    }
}

/// Resolve a repo to a [`BuildOutput`].
///
/// If the repo already contains `.vercel/output`, parse it. Otherwise this
/// returns the [`BuildPlan`] the caller must execute (run install + build), then
/// the caller should call [`synthesize`] on the produced output directory.
pub enum Resolution {
    /// Build Output already present — provision straight from it.
    Ready(BuildOutput),
    /// Must run these commands, then synthesize from `output_dir`.
    NeedsBuild(BuildPlan),
}

pub fn resolve_build_output_checked(
    repo: &Path,
) -> Result<Option<BuildOutput>, BuildContractError> {
    let root = repo.join(".vercel/output");
    let metadata = match std::fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BuildContractError::new(
                BuildContractErrorCode::InvalidBuildOutput,
                "inspect Build Output API v3",
                error.to_string(),
            ));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(BuildContractError::new(
            BuildContractErrorCode::InvalidBuildOutput,
            "inspect Build Output API v3",
            ".vercel/output must be a real directory",
        ));
    }
    let output = parse_build_output(repo).map_err(|error| {
        BuildContractError::new(
            BuildContractErrorCode::InvalidBuildOutput,
            "parse Build Output API v3",
            error.to_string(),
        )
    })?;
    if output.config.version != BUILD_OUTPUT_VERSION {
        return Err(BuildContractError::new(
            BuildContractErrorCode::InvalidBuildOutput,
            "parse Build Output API v3",
            format!(
                "config.json declares version {}; only version {} is supported",
                output.config.version, BUILD_OUTPUT_VERSION
            ),
        ));
    }
    Ok(Some(output))
}

/// Resolve build planning and any checked-in Build Output through one fail-closed
/// entry point. `package_manager` is the already-validated install/workspace-root
/// snapshot; framework detection and outputDirectory remain selected-app local.
pub fn resolve_build_checked(
    repo: &Path,
    package_manager: &PackageManagerDetection,
    framework_override: Option<&str>,
    install_override: Option<&str>,
    build_override: Option<&str>,
    output_override: Option<&str>,
) -> Result<CheckedBuildResolution, BuildContractError> {
    let plan = plan_build_checked_with_package_manager(
        repo,
        package_manager,
        framework_override,
        install_override,
        build_override,
        output_override,
    )?;
    let build_output = resolve_build_output_checked(repo)?;
    Ok(CheckedBuildResolution { plan, build_output })
}

pub fn resolve(repo: &Path) -> anyhow::Result<Resolution> {
    let package_manager = detect_package_manager_checked(repo)
        .map_err(|error| BuildContractError::invalid_metadata("resolve package metadata", error))?;
    let resolution = resolve_build_checked(repo, &package_manager, None, None, None, None)?;
    match resolution.build_output {
        Some(output) => Ok(Resolution::Ready(output)),
        None => Ok(Resolution::NeedsBuild(resolution.plan)),
    }
}

/// Synthesize a Build Output config for a framework whose native output is a
/// static directory (Vite/CRA/Astro/static) or a single serverless server
/// (Node). This is the minimal "adapter" that maps non-`.vercel/output`
/// frameworks into the standard contract.
///
/// `static_dir` is the framework's output directory (e.g. `dist`, `build`,
/// `out`). For SPA frameworks we add a catch-all rewrite to `index.html`.
pub fn synthesize_config(primitive: Primitive) -> BuildOutputConfig {
    let mut cfg = BuildOutputConfig::default();
    match primitive {
        Primitive::Static => {
            // Serve files from the filesystem; SPA fallback to index.html.
            cfg.routes = vec![
                Route {
                    handle: Some("filesystem".into()),
                    ..Default::default()
                },
                Route {
                    src: Some("/(.*)".into()),
                    dest: Some("/index.html".into()),
                    ..Default::default()
                },
            ];
        }
        Primitive::Serverless | Primitive::Hybrid => {
            // Static assets first, then everything else hits the function.
            cfg.routes = vec![
                Route {
                    handle: Some("filesystem".into()),
                    ..Default::default()
                },
                Route {
                    src: Some("/(.*)".into()),
                    dest: Some("/index".into()),
                    ..Default::default()
                },
            ];
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn analyze_reports_framework_and_commands() {
        let dir = std::env::temp_dir().join(format!("an-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"next":"14"}}"#,
        )
        .unwrap();

        let a = analyze(&dir);
        assert_eq!(a.framework_slug, "nextjs");
        assert_eq!(a.build_command, "next build");
        assert!(!a.has_build_output);
        assert_eq!(a.primitive, Primitive::Hybrid);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn synthesize_static_has_spa_fallback() {
        let cfg = synthesize_config(Primitive::Static);
        assert!(cfg
            .routes
            .iter()
            .any(|r| r.dest.as_deref() == Some("/index.html")));
    }
}
