use crate::app_discovery::{BuildContract, WorkspaceOrchestrator};
use crate::build_coordinates::MonorepoCoordinates;
use crate::workspace::Workspace;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(crate) struct SnapshotInput<'a> {
    pub(crate) checkout_root: &'a Path,
    pub(crate) workspace: Option<&'a Workspace>,
    pub(crate) application_source: &'a str,
    pub(crate) application_evidence: Vec<String>,
    pub(crate) application_decision_digest: Option<String>,
    pub(crate) package_manager: &'a fluid_build::PackageManagerDetection,
    pub(crate) framework_source: &'a str,
    pub(crate) plan: &'a fluid_build::BuildPlan,
    pub(crate) install_override: Option<String>,
    pub(crate) build_override: Option<String>,
    pub(crate) build_contract: &'a BuildContract,
    pub(crate) coordinates: &'a MonorepoCoordinates,
    pub(crate) use_npm_ci: bool,
}

pub(crate) async fn snapshot(
    input: SnapshotInput<'_>,
) -> anyhow::Result<fluid_build::RepositoryBuildSnapshot> {
    let selected = fluid_build::RepositoryPath::parse(input.coordinates.selected_app_relative())?;
    let install = fluid_build::RepositoryPath::parse(input.coordinates.install_root_relative())?;
    let build_cwd = fluid_build::RepositoryPath::parse(input.coordinates.build_cwd_relative())?;
    let output_root = fluid_build::RepositoryPath::parse(input.coordinates.output_root_relative())?;
    let runtime_artifact_base =
        fluid_build::RepositoryPath::parse(input.coordinates.runtime_artifact_relative())?;
    let workspace = input
        .workspace
        .map(|workspace| fluid_build::WorkspaceSnapshot::new(workspace.source, &workspace.members))
        .transpose()?;

    let install_step =
        if let Some(explicit) = fluid_build::StepAuthority::explicit(input.install_override)? {
            explicit
        } else if input
            .coordinates
            .install_root()
            .join("package.json")
            .is_file()
        {
            fluid_build::StepAuthority::Generated(fluid_build::GeneratedStep::Install {
                use_npm_ci: input.use_npm_ci,
            })
        } else {
            fluid_build::StepAuthority::None
        };
    let build_step =
        if let Some(explicit) = fluid_build::StepAuthority::explicit(input.build_override)? {
            explicit
        } else {
            match input.build_contract {
                BuildContract::WorkspaceRoot {
                    orchestrator: WorkspaceOrchestrator::Turbo,
                } => {
                    let depth = input
                        .coordinates
                        .selected_app_relative()
                        .components()
                        .count();
                    fluid_build::StepAuthority::Generated(fluid_build::GeneratedStep::TurboBuild {
                        repository_from_app: fluid_build::ParentPath::from_depth(depth)?,
                    })
                }
                BuildContract::SelectedApp => fluid_build::StepAuthority::Generated(
                    fluid_build::GeneratedStep::RunBuildScript,
                ),
                BuildContract::FrameworkDefault if input.plan.framework.slug == "node" => {
                    fluid_build::StepAuthority::None
                }
                BuildContract::FrameworkDefault => {
                    let command = input.plan.framework.build_command.trim();
                    if command.is_empty() {
                        fluid_build::StepAuthority::None
                    } else if command == "npm run build" {
                        fluid_build::StepAuthority::Generated(
                            fluid_build::GeneratedStep::RunBuildScript,
                        )
                    } else {
                        let argv = command
                            .split_ascii_whitespace()
                            .map(str::to_string)
                            .collect::<Vec<_>>();
                        fluid_build::StepAuthority::Generated(
                            fluid_build::GeneratedStep::FrameworkExec {
                                argv: fluid_build::FixedArgv::parse(argv)?,
                            },
                        )
                    }
                }
            }
        };

    let metadata = metadata_inputs(
        input.checkout_root,
        input.coordinates.selected_app_relative(),
        input.workspace,
    )
    .await?;
    fluid_build::RepositoryBuildSnapshot::new(fluid_build::RepositoryBuildContract {
        schema: 0,
        package_manager: input.package_manager.into(),
        workspace,
        application: fluid_build::ApplicationSnapshot {
            selected: selected.clone(),
            source: input.application_source.to_string(),
            evidence: input.application_evidence,
            decision_digest: input.application_decision_digest,
        },
        framework: fluid_build::FrameworkSnapshot {
            slug: input.plan.framework.slug.to_string(),
            name: input.plan.framework.name.to_string(),
            source: input.framework_source.to_string(),
        },
        steps: fluid_build::BuildSteps {
            install: install_step,
            build: build_step,
        },
        output: input.plan.output_dir.clone(),
        coordinates: fluid_build::RepositoryCoordinates {
            checkout_root: fluid_build::RepositoryPath::root(),
            install_root: install,
            selected_app: selected,
            build_cwd,
            output_root,
            runtime_artifact_base,
            function_cwd: fluid_build::RepositoryPath::root(),
        },
        metadata,
    })
    .map_err(Into::into)
}

async fn metadata_inputs(
    checkout_root: &Path,
    selected_relative: &Path,
    workspace: Option<&Workspace>,
) -> anyhow::Result<Vec<fluid_build::MetadataInputSeal>> {
    let mut paths = BTreeSet::new();
    for name in [
        "package.json",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
        "yarn.lock",
        ".yarnrc.yml",
        "bun.lock",
        "bun.lockb",
        "vercel.json",
        "fluid.json",
        "next.config.js",
        "next.config.mjs",
        "next.config.ts",
        "nuxt.config.js",
        "nuxt.config.ts",
        "svelte.config.js",
        "astro.config.mjs",
        "astro.config.js",
        "astro.config.ts",
        "gatsby-config.js",
        "gatsby-config.ts",
        "remix.config.js",
        "vite.config.js",
        "vite.config.ts",
        "vue.config.js",
        "open-next.config.ts",
        "open-next.config.js",
        "open-next.config.mjs",
    ] {
        paths.insert(PathBuf::from(name));
        if !selected_relative.as_os_str().is_empty() {
            paths.insert(selected_relative.join(name));
        }
    }
    if let Some(workspace) = workspace {
        for member in &workspace.members {
            paths.insert(member.join("package.json"));
        }
    }
    let mut metadata = Vec::new();
    for relative in paths {
        let label = relative.to_string_lossy();
        let Some(bytes) =
            crate::workspace::read_bounded(&checkout_root.join(&relative), &label).await?
        else {
            continue;
        };
        metadata.push(fluid_build::MetadataInputSeal {
            path: fluid_build::RepositoryPath::parse(&relative)?,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
        });
    }
    Ok(metadata)
}
