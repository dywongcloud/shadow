use anyhow::Context;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

const DISCOVERY_POLICY_VERSION: u16 = 1;

pub(crate) struct Selection {
    pub(crate) relative: PathBuf,
    pub(crate) evidence: Vec<String>,
    pub(crate) workspace_source: &'static str,
    pub(crate) decision_digest: String,
}

#[derive(serde::Serialize)]
struct DiscoveryCandidateV1 {
    relative: String,
    score: u8,
    evidence: Vec<String>,
}

#[derive(serde::Serialize)]
struct DiscoveryReportV1 {
    policy_version: u16,
    declaration_source: String,
    effective_members: Vec<String>,
    candidates: Vec<DiscoveryCandidateV1>,
    selected: String,
}

struct Candidate {
    relative: PathBuf,
    evidence: Vec<String>,
    score: u8,
}

pub(crate) async fn select(root: &Path) -> anyhow::Result<Option<Selection>> {
    // Explicit root contracts are authoritative. A direct root application
    // (prebuilt output, recognized framework, or start script) also wins before
    // workspace inference; only a bare root index participates in member ranking.
    if has_root_contract(root).await? {
        return Ok(None);
    }
    let Some(workspace) = crate::workspace::load(root).await? else {
        return Ok(None);
    };
    workspace_package_names(root, &workspace).await?;

    let unsupported = unsupported_root_markers(root).await?;
    let mut candidates = Vec::new();
    if let Some((score, evidence)) = classify(root, Path::new("")).await? {
        candidates.push(Candidate {
            relative: PathBuf::new(),
            evidence,
            score,
        });
    }
    for relative in &workspace.members {
        if let Some((score, evidence)) = classify(&root.join(relative), relative).await? {
            candidates.push(Candidate {
                relative: relative.clone(),
                evidence,
                score,
            });
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| path_key(&left.relative).cmp(&path_key(&right.relative)))
    });

    if !unsupported.is_empty()
        && candidates
            .iter()
            .any(|candidate| !candidate.relative.as_os_str().is_empty())
    {
        anyhow::bail!(
            "repository root contains unsupported application marker(s) [{}]; refusing to guess that a nested JavaScript workspace is the intended service — set Root Directory or add Dockerfile/fluid.json",
            unsupported.join(", ")
        );
    }
    anyhow::ensure!(
        !candidates.is_empty(),
        "{} declares a workspace, but no deployable application was found; set Root Directory explicitly",
        workspace.source
    );
    let candidate_reports = candidates
        .iter()
        .map(|candidate| DiscoveryCandidateV1 {
            relative: display_relative(&candidate.relative),
            score: candidate.score,
            evidence: candidate.evidence.clone(),
        })
        .collect::<Vec<_>>();
    let direct_root = candidates
        .iter()
        .position(|candidate| candidate.relative.as_os_str().is_empty() && candidate.score > 1);
    let candidate = if let Some(index) = direct_root {
        // A real root application contract (prebuilt output, recognized framework,
        // or start script) wins before workspace inference. A bare root index.html
        // remains weak evidence so it cannot hide a stronger nested application.
        candidates.remove(index)
    } else {
        let winner_score = candidates[0].score;
        candidates.retain(|candidate| candidate.score == winner_score);
        if candidates.len() > 1 {
            let choices = candidates
                .iter()
                .map(|candidate| {
                    format!(
                        "{} [{}]",
                        display_relative(&candidate.relative),
                        candidate.evidence.join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!(
                "multiple deployable workspace applications were found: {choices}; set Root Directory explicitly"
            );
        }
        candidates.remove(0)
    };
    let report = DiscoveryReportV1 {
        policy_version: DISCOVERY_POLICY_VERSION,
        declaration_source: workspace.source.to_string(),
        effective_members: workspace
            .members
            .iter()
            .map(|member| display_relative(member))
            .collect(),
        candidates: candidate_reports,
        selected: display_relative(&candidate.relative),
    };
    let encoded = serde_json::to_vec(&report).context("encode workspace discovery report")?;
    let decision_digest = Sha256::digest(encoded)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(Some(Selection {
        relative: candidate.relative,
        evidence: candidate.evidence,
        workspace_source: workspace.source,
        decision_digest,
    }))
}

pub(crate) async fn is_member(root: &Path, candidate: &Path) -> anyhow::Result<bool> {
    let Some(workspace) = crate::workspace::load(root).await? else {
        return Ok(false);
    };
    let relative = candidate.strip_prefix(root).map_err(|_| {
        anyhow::anyhow!(
            "selected root '{}' is not inside checkout '{}'",
            candidate.display(),
            root.display()
        )
    })?;
    Ok(!relative.as_os_str().is_empty()
        && workspace.members.iter().any(|member| member == relative))
}

pub(crate) async fn runtime_include_rel(
    root: &Path,
    selected: &Path,
) -> anyhow::Result<hive_backend::runtime_artifact::RuntimeArtifactClosure> {
    let selected_relative = selected.strip_prefix(root).map_err(|_| {
        anyhow::anyhow!(
            "selected application '{}' is not inside checkout '{}'",
            selected.display(),
            root.display()
        )
    })?;
    let Some(workspace) = crate::workspace::load(root).await? else {
        return Ok(Default::default());
    };
    if selected_relative.as_os_str().is_empty()
        || !workspace
            .members
            .iter()
            .any(|member| member == selected_relative)
    {
        // The app root already carries its own node_modules, while a nested
        // non-member is installed independently in its own directory.
        return Ok(Default::default());
    }

    let Some(selected_package) = read_package(selected, selected_relative).await? else {
        // A checked-in/prebuilt deployment contract may be a complete artifact
        // without package metadata. It has no declared workspace dependency
        // closure; the selected application itself is staged separately.
        return Ok(Default::default());
    };

    let mut include = BTreeSet::new();
    match tokio::fs::symlink_metadata(root.join("node_modules")).await {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "workspace root node_modules must be a real directory"
            );
            include.insert(PathBuf::from("node_modules"));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let members_by_name = workspace_package_names(root, &workspace).await?;
    let mut pending = runtime_dependency_names(&selected_package)?
        .into_iter()
        .collect::<VecDeque<_>>();
    let mut runtime_packages = vec![selected_package];
    let mut visited_names = BTreeSet::new();
    while let Some(name) = pending.pop_front() {
        if !visited_names.insert(name.clone()) {
            continue;
        }
        let Some(member) = members_by_name.get(&name) else {
            continue;
        };
        if member != selected_relative && include.insert(member.clone()) {
            let package = read_package(&root.join(member), member)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "workspace member {} lost package.json",
                        display_relative(member)
                    )
                })?;
            pending.extend(runtime_dependency_names(&package)?.into_iter());
            runtime_packages.push(package);
        }
    }

    let mut omitted = BTreeSet::new();
    for package in &runtime_packages {
        for (link_name, target_name) in development_workspace_links(package)? {
            let Some(member) = members_by_name.get(&target_name) else {
                continue;
            };
            if member == selected_relative || include.contains(member) {
                continue;
            }
            omitted.insert((link_name, target_name, member.clone()));
        }
    }
    let omitted_workspace_links = omitted
        .into_iter()
        .map(|(link_name, target_name, target_rel)| {
            hive_backend::runtime_artifact::RuntimeArtifactWorkspaceLink::new(
                link_name,
                target_name,
                target_rel,
            )
        })
        .collect();
    Ok(hive_backend::runtime_artifact::RuntimeArtifactClosure::new(
        include.into_iter().collect(),
        omitted_workspace_links,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceOrchestrator {
    Turbo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BuildContract {
    WorkspaceRoot {
        orchestrator: WorkspaceOrchestrator,
        package_name: String,
    },
    SelectedApp,
    FrameworkDefault,
}

pub(crate) async fn build_contract(
    root: &Path,
    selected: &Path,
    workspace_member: bool,
    workspace: Option<&crate::workspace::Workspace>,
    explicit_build_command: bool,
) -> anyhow::Result<BuildContract> {
    if explicit_build_command {
        return Ok(BuildContract::FrameworkDefault);
    }
    if selected != root && workspace_member {
        let relative = selected.strip_prefix(root).map_err(|_| {
            anyhow::anyhow!(
                "selected application '{}' is not inside checkout '{}'",
                selected.display(),
                root.display()
            )
        })?;
        if let Some(package) = read_package(root, Path::new("")).await? {
            if let Some(build) = package
                .get("scripts")
                .and_then(serde_json::Value::as_object)
                .and_then(|scripts| scripts.get("build"))
            {
                let build = build.as_str().ok_or_else(|| {
                    anyhow::anyhow!("repository root scripts.build must be a string")
                })?;
                if let Some(orchestrator) = workspace_orchestrator(build)? {
                    let workspace = workspace.ok_or_else(|| {
                        anyhow::anyhow!(
                            "selected application {} was marked as a workspace member, but no checked workspace snapshot exists",
                            display_relative(relative)
                        )
                    })?;
                    let names = workspace_package_names(root, workspace).await?;
                    let package_name = names
                        .iter()
                        .find_map(|(name, member)| (member == relative).then(|| name.clone()))
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "selected workspace application {} must declare a unique package.json name for filtered builds",
                                display_relative(relative)
                            )
                        })?;
                    return Ok(BuildContract::WorkspaceRoot {
                        orchestrator,
                        package_name,
                    });
                }
            }
        }
    }
    let relative = selected.strip_prefix(root).map_err(|_| {
        anyhow::anyhow!(
            "selected application '{}' is not inside checkout '{}'",
            selected.display(),
            root.display()
        )
    })?;
    if has_build_script(selected, relative).await? {
        Ok(BuildContract::SelectedApp)
    } else {
        Ok(BuildContract::FrameworkDefault)
    }
}

pub(crate) async fn has_build_script(dir: &Path, relative: &Path) -> anyhow::Result<bool> {
    let Some(package) = read_package(dir, relative).await? else {
        return Ok(false);
    };
    let Some(scripts) = package.get("scripts") else {
        return Ok(false);
    };
    let scripts = scripts.as_object().ok_or_else(|| {
        anyhow::anyhow!("{} scripts must be an object", display_relative(relative))
    })?;
    let Some(build) = scripts.get("build") else {
        return Ok(false);
    };
    let build = build.as_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} scripts.build must be a string",
            display_relative(relative)
        )
    })?;
    Ok(!build.trim().is_empty())
}

fn workspace_orchestrator(command: &str) -> anyhow::Result<Option<WorkspaceOrchestrator>> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let turbo = matches!(
        words.as_slice(),
        ["turbo", "run", "build"]
            | ["turbo", "build"]
            | ["npx", "turbo", "run", "build"]
            | ["npx", "turbo", "build"]
            | ["npm", "exec", "turbo", "run", "build"]
            | ["npm", "exec", "turbo", "build"]
            | ["pnpm", "turbo", "run", "build"]
            | ["pnpm", "turbo", "build"]
            | ["pnpm", "exec", "turbo", "run", "build"]
            | ["pnpm", "exec", "turbo", "build"]
            | ["yarn", "turbo", "run", "build"]
            | ["yarn", "turbo", "build"]
            | ["bunx", "turbo", "run", "build"]
            | ["bunx", "turbo", "build"]
            | ["bunx", "--bun", "turbo", "run", "build"]
            | ["bunx", "--bun", "turbo", "build"]
    );
    if turbo {
        return Ok(Some(WorkspaceOrchestrator::Turbo));
    }
    anyhow::ensure!(
        !is_workspace_orchestrator(command),
        "unsupported repository root workspace build script {command:?}; set Build Command explicitly"
    );
    Ok(None)
}

async fn has_root_contract(root: &Path) -> anyhow::Result<bool> {
    let mut found = false;
    for name in [
        "compose.yaml",
        "compose.yml",
        "docker-compose.yaml",
        "docker-compose.yml",
        "Dockerfile",
        "Containerfile",
        "fluid.json",
    ] {
        let path = root.join(name);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_file(),
                    "root contract {name} must be a regular file, not a symlink or directory"
                );
                found = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(found)
}

async fn unsupported_root_markers(root: &Path) -> anyhow::Result<Vec<&'static str>> {
    let mut found = Vec::new();
    for name in [
        "pyproject.toml",
        "requirements.txt",
        "Pipfile",
        "Cargo.toml",
        "go.mod",
        "Gemfile",
        "mix.exs",
    ] {
        match tokio::fs::symlink_metadata(root.join(name)).await {
            Ok(metadata) if metadata.file_type().is_file() => found.push(name),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("root application marker {name} may not be a symlink")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(found)
}

async fn classify(dir: &Path, relative: &Path) -> anyhow::Result<Option<(u8, Vec<String>)>> {
    let package = read_package(dir, relative).await?;
    let contracts = application_contracts(dir, relative).await?;
    if !relative.as_os_str().is_empty() && package.is_none() && contracts.is_empty() {
        return Ok(None);
    }
    let mut frameworks = BTreeSet::new();
    let mut start = false;
    if let Some(package) = package.as_ref() {
        let dependencies = dependency_names(package)?;
        let has = |name: &str| dependencies.contains(name);
        if has("@opennextjs/aws") || has("open-next") {
            frameworks.insert("opennext");
        }
        if has("vinext") {
            frameworks.insert("vinext");
        }
        if has("next") {
            frameworks.insert("nextjs");
        }
        if has("nuxt") || has("nuxt3") {
            frameworks.insert("nuxtjs");
        }
        if has("@sveltejs/kit") {
            frameworks.insert("sveltekit");
        }
        if has("@remix-run/dev") {
            frameworks.insert("remix");
        }
        if has("astro") {
            frameworks.insert("astro");
        }
        if has("gatsby") {
            frameworks.insert("gatsby");
        }
        if has("react-scripts") {
            frameworks.insert("create-react-app");
        }
        if has("@vue/cli-service") {
            frameworks.insert("vue");
        }
        if has("vite") {
            frameworks.insert("vite");
        }
        if let Some(scripts) = package.get("scripts") {
            let scripts = scripts.as_object().ok_or_else(|| {
                anyhow::anyhow!("{} scripts must be an object", display_relative(relative))
            })?;
            for name in ["start", "build"] {
                if let Some(value) = scripts.get(name) {
                    let value = value.as_str().ok_or_else(|| {
                        anyhow::anyhow!(
                            "{} scripts.{name} must be a string",
                            display_relative(relative)
                        )
                    })?;
                    if name == "start"
                        && !value.trim().is_empty()
                        && (!relative.as_os_str().is_empty() || !is_workspace_orchestrator(value))
                    {
                        start = true;
                    }
                }
            }
        }
    }

    if frameworks.contains("opennext") {
        frameworks.remove("nextjs");
    }
    if frameworks.contains("vinext") {
        frameworks.remove("nextjs");
        frameworks.remove("vite");
    }
    if frameworks.contains("sveltekit")
        || frameworks.contains("remix")
        || frameworks.contains("astro")
    {
        frameworks.remove("vite");
    }
    anyhow::ensure!(
        frameworks.len() <= 1,
        "{} contains conflicting framework evidence: {}",
        display_relative(relative),
        frameworks.iter().copied().collect::<Vec<_>>().join(", ")
    );

    let static_index = regular_marker(dir, relative, "index.html").await?;
    if frameworks.is_empty() && !start && !static_index && contracts.is_empty() {
        return Ok(None);
    }
    let score = if !contracts.is_empty() {
        // A checked-in deployment contract or prebuilt Build Output is stronger
        // than inference from package metadata.
        4
    } else if !frameworks.is_empty() {
        3
    } else if start {
        2
    } else {
        1
    };
    let mut evidence = contracts;
    evidence.extend(
        frameworks
            .into_iter()
            .map(|framework| format!("framework:{framework}")),
    );
    if start {
        evidence.push("script:start".to_string());
    }
    if static_index {
        evidence.push("static:index.html".to_string());
    }
    Ok(Some((score, evidence)))
}

fn is_workspace_orchestrator(command: &str) -> bool {
    let words = command.split_whitespace().collect::<Vec<_>>();
    matches!(
        words.as_slice(),
        ["turbo", ..]
            | ["nx", ..]
            | ["lerna", ..]
            | ["npx" | "bunx", "turbo" | "nx" | "lerna", ..]
            | ["bunx", "--bun", "turbo" | "nx" | "lerna", ..]
            | ["npm", "exec", "turbo" | "nx" | "lerna", ..]
            | ["pnpm", "turbo" | "nx" | "lerna", ..]
            | ["pnpm", "exec", "turbo" | "nx" | "lerna", ..]
            | ["pnpm", "-r" | "--recursive", ..]
            | ["yarn", "turbo" | "nx" | "lerna", ..]
            | ["yarn", "workspaces", ..]
    ) || (words.first() == Some(&"npm")
        && words
            .iter()
            .any(|word| *word == "--workspaces" || *word == "--workspace"))
}

async fn application_contracts(dir: &Path, relative: &Path) -> anyhow::Result<Vec<String>> {
    let mut evidence = Vec::new();
    for name in [
        "fluid.json",
        "Dockerfile",
        "Containerfile",
        "compose.yaml",
        "compose.yml",
        "docker-compose.yaml",
        "docker-compose.yml",
        ".vercel/output/config.json",
    ] {
        if regular_marker(dir, relative, name).await? {
            evidence.push(format!("contract:{name}"));
        }
    }
    Ok(evidence)
}

async fn workspace_package_names(
    root: &Path,
    workspace: &crate::workspace::Workspace,
) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let mut names = BTreeMap::new();
    for member in &workspace.members {
        let Some(package) = read_package(&root.join(member), member).await? else {
            continue;
        };
        let Some(name) = package.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        anyhow::ensure!(
            safe_package_name(name),
            "workspace member {} declares package name {name:?}, which cannot be used as an exact build filter",
            display_relative(member)
        );
        anyhow::ensure!(
            names.insert(name.to_string(), member.clone()).is_none(),
            "workspace package name {name:?} is declared by more than one member; set Root Directory explicitly"
        );
    }
    Ok(names)
}

fn safe_package_name(name: &str) -> bool {
    fn segment(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 214
            && value.as_bytes()[0].is_ascii_alphanumeric()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    }

    if name.is_empty() || name.len() > 214 || !name.is_ascii() {
        return false;
    }
    if let Some(scoped) = name.strip_prefix('@') {
        let Some((scope, package)) = scoped.split_once('/') else {
            return false;
        };
        !package.contains('/') && segment(scope) && segment(package)
    } else {
        !name.contains('/') && !name.contains('@') && segment(name)
    }
}

async fn read_package(dir: &Path, relative: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    let label = format!("{} package.json", display_relative(relative));
    let Some(bytes) = crate::workspace::read_bounded(&dir.join("package.json"), &label).await?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        anyhow::anyhow!(
            "invalid {} package.json: {error}",
            display_relative(relative)
        )
    })
}

fn runtime_dependency_names(package: &serde_json::Value) -> anyhow::Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for field in ["dependencies", "peerDependencies", "optionalDependencies"] {
        let Some(value) = package.get(field) else {
            continue;
        };
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("package.json {field} must be an object"))?;
        for (key, value) in object {
            let spec = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("package.json {field}.{key} must be a string"))?;
            let mut target = key.as_str();
            if let Some(workspace_spec) = spec.strip_prefix("workspace:") {
                if let Some((alias, selector)) = workspace_spec.rsplit_once('@') {
                    if !alias.is_empty() {
                        anyhow::ensure!(
                            !selector.is_empty() && safe_package_name(alias),
                            "package.json {field}.{key} has malformed workspace alias {spec:?}"
                        );
                        target = alias;
                    }
                }
            }
            names.insert(target.to_string());
        }
    }
    Ok(names)
}

fn development_workspace_links(
    package: &serde_json::Value,
) -> anyhow::Result<BTreeSet<(String, String)>> {
    let Some(value) = package.get("devDependencies") else {
        return Ok(BTreeSet::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("package.json devDependencies must be an object"))?;
    let mut links = BTreeSet::new();
    for (link_name, value) in object {
        let spec = value.as_str().ok_or_else(|| {
            anyhow::anyhow!("package.json devDependencies.{link_name} must be a string")
        })?;
        let Some(workspace_spec) = spec.strip_prefix("workspace:") else {
            continue;
        };
        anyhow::ensure!(
            safe_package_name(link_name),
            "package.json devDependencies contains malformed workspace link name {link_name:?}"
        );
        let mut target_name = link_name.as_str();
        if let Some((alias, selector)) = workspace_spec.rsplit_once('@') {
            if !alias.is_empty() {
                anyhow::ensure!(
                    !selector.is_empty() && safe_package_name(alias),
                    "package.json devDependencies.{link_name} has malformed workspace alias {spec:?}"
                );
                target_name = alias;
            }
        }
        links.insert((link_name.clone(), target_name.to_string()));
    }
    Ok(links)
}

fn dependency_names(package: &serde_json::Value) -> anyhow::Result<BTreeSet<&str>> {
    let mut names = BTreeSet::new();
    for field in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        let Some(value) = package.get(field) else {
            continue;
        };
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("package.json {field} must be an object"))?;
        names.extend(object.keys().map(String::as_str));
    }
    Ok(names)
}

async fn regular_marker(dir: &Path, relative: &Path, name: &str) -> anyhow::Result<bool> {
    let mut path = dir.to_path_buf();
    let components = Path::new(name).components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let component = match component {
            std::path::Component::Normal(component) => *component,
            _ => anyhow::bail!("application marker {name:?} is not relative and normalized"),
        };
        path.push(component);
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "{} marker {name} may not resolve through a symlink",
            display_relative(relative)
        );
        if index + 1 == components.len() {
            return Ok(metadata.file_type().is_file());
        }
        anyhow::ensure!(
            metadata.is_dir(),
            "{} marker {name} has a non-directory parent",
            display_relative(relative)
        );
    }
    Ok(false)
}

fn path_key(path: &Path) -> Vec<u8> {
    path.to_string_lossy().replace('\\', "/").into_bytes()
}

fn display_relative(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_string()
    } else {
        path.to_string_lossy().replace('\\', "/")
    }
}
