use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncReadExt;

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_PATTERNS: usize = 128;
const MAX_PATTERN_DEPTH: usize = 12;
const MAX_MEMBER_DEPTH: usize = 32;
const MAX_VISITED_ENTRIES: usize = 4096;
const MAX_MEMBERS: usize = 256;
const MAX_PATH_BYTES: usize = 4096;

const PRUNED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".next",
    ".nuxt",
    ".output",
    ".turbo",
    "node_modules",
    "target",
    "dist",
    "build",
    "coverage",
];

#[derive(Clone)]
enum Segment {
    Literal(String),
    One,
    Many,
}

#[derive(Clone)]
struct Pattern {
    raw: String,
    negative: bool,
    segments: Vec<Segment>,
}

pub(crate) struct Workspace {
    pub(crate) source: &'static str,
    pub(crate) members: Vec<PathBuf>,
}

pub(crate) async fn load(root: &Path) -> anyhow::Result<Option<Workspace>> {
    let (source, raw_patterns) = if let Some(bytes) =
        read_bounded(&root.join("pnpm-workspace.yaml"), "pnpm-workspace.yaml").await?
    {
        let value: serde_yaml::Value = serde_yaml::from_slice(&bytes)
            .map_err(|error| anyhow::anyhow!("invalid pnpm-workspace.yaml: {error}"))?;
        let packages = value
            .get("packages")
            .and_then(serde_yaml::Value::as_sequence)
            .ok_or_else(|| anyhow::anyhow!("pnpm-workspace.yaml must contain a packages array"))?;
        let mut patterns = Vec::with_capacity(packages.len());
        for value in packages {
            let pattern = value.as_str().ok_or_else(|| {
                anyhow::anyhow!("pnpm-workspace.yaml packages entries must all be strings")
            })?;
            patterns.push(pattern.to_string());
        }
        ("pnpm-workspace.yaml", patterns)
    } else {
        let Some(bytes) = read_bounded(&root.join("package.json"), "package.json").await? else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow::anyhow!("invalid repository package.json: {error}"))?;
        let Some(workspaces) = value.get("workspaces") else {
            return Ok(None);
        };
        let packages = if let Some(array) = workspaces.as_array() {
            array
        } else {
            workspaces
                .get("packages")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "package.json workspaces must be an array or an object with a packages array"
                    )
                })?
        };
        let mut patterns = Vec::with_capacity(packages.len());
        for value in packages {
            let pattern = value.as_str().ok_or_else(|| {
                anyhow::anyhow!("package.json workspace entries must all be strings")
            })?;
            patterns.push(pattern.to_string());
        }
        ("package.json#workspaces", patterns)
    };

    anyhow::ensure!(
        !raw_patterns.is_empty(),
        "{source} declares an empty workspace; set Root Directory explicitly"
    );
    anyhow::ensure!(
        raw_patterns.len() <= MAX_PATTERNS,
        "{source} declares {} workspace patterns; limit is {MAX_PATTERNS}",
        raw_patterns.len()
    );

    let patterns = raw_patterns
        .iter()
        .map(|raw| parse_pattern(raw))
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        patterns.iter().any(|pattern| !pattern.negative),
        "{source} contains no positive workspace pattern"
    );

    let mut visited_entries = 0usize;
    let mut expanded = BTreeSet::new();
    for pattern in patterns.iter().filter(|pattern| !pattern.negative) {
        for member in expand(root, pattern, &mut visited_entries).await? {
            expanded.insert(path_string(&member)?);
            anyhow::ensure!(
                expanded.len() <= MAX_MEMBERS,
                "workspace expansion exceeds {MAX_MEMBERS} members; set Root Directory explicitly"
            );
        }
    }

    let mut members = Vec::new();
    for member in expanded {
        let path = PathBuf::from(&member);
        let mut included = false;
        let mut excluded = false;
        for pattern in &patterns {
            if pattern_matches(pattern, &path)? {
                if pattern.negative {
                    excluded = true;
                } else {
                    included = true;
                }
            }
        }
        if included && !excluded {
            members.push(path);
        }
    }
    Ok(Some(Workspace { source, members }))
}

pub(crate) async fn validate(
    root: &Path,
    workspace: Option<&Workspace>,
    package_manager: &fluid_build::PackageManagerDetection,
) -> anyhow::Result<()> {
    if workspace.is_some_and(|workspace| workspace.source == "pnpm-workspace.yaml") {
        anyhow::ensure!(
            package_manager.manager == "pnpm",
            "pnpm-workspace.yaml requires pnpm, but the checked package-manager snapshot selects {} ({:?})",
            package_manager.manager,
            package_manager.source
        );
    }
    reject_yarn_pnp(root, package_manager).await
}

async fn reject_yarn_pnp(
    root: &Path,
    package_manager: &fluid_build::PackageManagerDetection,
) -> anyhow::Result<()> {
    for marker in [".pnp.cjs", ".pnp.js", ".pnp.loader.mjs"] {
        match tokio::fs::symlink_metadata(root.join(marker)).await {
            Ok(_) => anyhow::bail!(
                "Yarn Plug'n'Play marker {marker} is unsupported; configure nodeLinker: node-modules or set Root Directory explicitly"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if package_manager.manager != "yarn" {
        return Ok(());
    }

    let mut node_linker = None;
    let mut yarn_rc_present = false;
    if let Some(bytes) = read_bounded(&root.join(".yarnrc.yml"), ".yarnrc.yml").await? {
        yarn_rc_present = true;
        let value: serde_yaml::Value = serde_yaml::from_slice(&bytes)
            .map_err(|error| anyhow::anyhow!("invalid .yarnrc.yml: {error}"))?;
        node_linker = value
            .get("nodeLinker")
            .and_then(serde_yaml::Value::as_str)
            .map(|value| value.trim().to_ascii_lowercase());
        if let Some(linker) = node_linker.as_deref() {
            anyhow::ensure!(
                matches!(linker, "node-modules" | "pnpm"),
                "Yarn nodeLinker {linker:?} is unsupported; configure nodeLinker: node-modules"
            );
        }
    }

    anyhow::ensure!(
        !yarn_rc_present || node_linker.is_some(),
        "Yarn Plug'n'Play is the default when .yarnrc.yml omits nodeLinker; configure nodeLinker: node-modules"
    );
    if !yarn_rc_present {
        anyhow::ensure!(
            package_manager.lockfile != Some(fluid_build::PackageManagerLockfile::YarnModern)
                && package_manager
                    .declaration
                    .as_ref()
                    .is_none_or(|declaration| {
                        declaration
                            .version
                            .split('.')
                            .next()
                            .and_then(|value| value.parse::<u64>().ok())
                            == Some(1)
                    }),
            "Yarn defaults to Plug'n'Play for the checked package-manager version; configure nodeLinker: node-modules"
        );
    }
    Ok(())
}

pub(crate) async fn read_bounded(path: &Path, label: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = match options.open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            anyhow::bail!("{label} must be a regular file, not a symlink")
        }
        Err(error) => return Err(anyhow::anyhow!("could not open {label}: {error}")),
    };
    let before = file
        .metadata()
        .await
        .map_err(|error| anyhow::anyhow!("could not inspect {label}: {error}"))?;
    anyhow::ensure!(
        before.file_type().is_file(),
        "{label} must be a regular file, not a symlink or directory"
    );
    anyhow::ensure!(
        before.len() <= MAX_MANIFEST_BYTES,
        "{label} is {} bytes; limit is {MAX_MANIFEST_BYTES}",
        before.len()
    );

    let mut bytes = Vec::with_capacity(before.len() as usize);
    {
        let mut limited = (&mut file).take(MAX_MANIFEST_BYTES + 1);
        limited
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| anyhow::anyhow!("could not read {label}: {error}"))?;
    }
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "{label} grew beyond {MAX_MANIFEST_BYTES} bytes while it was read"
    );
    let after = file
        .metadata()
        .await
        .map_err(|error| anyhow::anyhow!("could not re-inspect {label}: {error}"))?;
    anyhow::ensure!(
        before.len() == after.len() && before.modified().ok() == after.modified().ok(),
        "{label} changed while it was read; retry the build"
    );
    Ok(Some(bytes))
}

fn parse_pattern(raw: &str) -> anyhow::Result<Pattern> {
    let raw = raw.trim();
    anyhow::ensure!(!raw.is_empty(), "workspace patterns may not be empty");
    anyhow::ensure!(
        raw.len() <= MAX_PATH_BYTES && !raw.chars().any(char::is_control),
        "workspace pattern {:?} contains control characters or exceeds {MAX_PATH_BYTES} bytes",
        raw
    );
    anyhow::ensure!(
        !raw.contains('\\'),
        "workspace pattern {:?} uses backslashes; only '/' separators are supported",
        raw
    );
    let (negative, body) = raw
        .strip_prefix('!')
        .map(|body| (true, body))
        .unwrap_or((false, raw));
    let body = body.strip_prefix("./").unwrap_or(body);
    anyhow::ensure!(
        !body.is_empty() && !Path::new(body).is_absolute(),
        "workspace pattern {:?} must be repository-relative",
        raw
    );

    let mut segments = Vec::new();
    for component in Path::new(body).components() {
        let value = match component {
            Component::Normal(value) => value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("workspace pattern {:?} is not valid UTF-8", raw))?,
            _ => {
                anyhow::bail!(
                    "workspace pattern {:?} contains '.' or '..'; set Root Directory explicitly",
                    raw
                )
            }
        };
        if value == "*" {
            segments.push(Segment::One);
        } else if value == "**" {
            segments.push(Segment::Many);
        } else {
            anyhow::ensure!(
                !value.contains(['*', '?', '[', ']', '{', '}']),
                "workspace pattern {:?} uses unsupported glob syntax; supported wildcards are whole-segment '*' and '**'",
                raw
            );
            anyhow::ensure!(
                negative || !PRUNED_DIRS.contains(&value),
                "workspace pattern {:?} enters pruned directory {value:?}",
                raw
            );
            segments.push(Segment::Literal(value.to_string()));
        }
    }
    anyhow::ensure!(
        !segments.is_empty() && segments.len() <= MAX_PATTERN_DEPTH,
        "workspace pattern {:?} exceeds depth {MAX_PATTERN_DEPTH}",
        raw
    );
    Ok(Pattern {
        raw: raw.to_string(),
        negative,
        segments,
    })
}

async fn expand(
    root: &Path,
    pattern: &Pattern,
    visited_entries: &mut usize,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = vec![PathBuf::new()];
    for segment in &pattern.segments {
        let mut next = Vec::new();
        for path in paths {
            match segment {
                Segment::Literal(name) => {
                    let candidate = path.join(name);
                    anyhow::ensure!(
                        candidate.components().count() <= MAX_MEMBER_DEPTH,
                        "workspace expansion exceeds directory depth {MAX_MEMBER_DEPTH} at {:?}; set Root Directory explicitly",
                        candidate
                    );
                    path_string(&candidate)?;
                    match tokio::fs::symlink_metadata(root.join(&candidate)).await {
                        Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
                            "workspace pattern {:?} resolves through symlink {:?}",
                            pattern.raw,
                            candidate
                        ),
                        Ok(metadata) if metadata.is_dir() => next.push(candidate),
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Segment::One => {
                    next.extend(child_dirs(root, &path, pattern, visited_entries).await?);
                }
                Segment::Many => {
                    next.extend(descendants(root, &path, pattern, visited_entries).await?);
                }
            }
        }
        next.sort_by_key(|path| path.to_string_lossy().into_owned());
        next.dedup();
        anyhow::ensure!(
            next.len() <= MAX_VISITED_ENTRIES,
            "workspace expansion exceeds {MAX_VISITED_ENTRIES} intermediate directories; set Root Directory explicitly"
        );
        paths = next;
    }
    Ok(paths)
}

async fn child_dirs(
    root: &Path,
    relative: &Path,
    pattern: &Pattern,
    visited_entries: &mut usize,
) -> anyhow::Result<Vec<PathBuf>> {
    let directory = root.join(relative);
    let mut reader = tokio::fs::read_dir(&directory).await.map_err(|error| {
        anyhow::anyhow!(
            "could not expand workspace pattern {:?} at {:?}: {error}",
            pattern.raw,
            relative
        )
    })?;
    let mut children = Vec::new();
    while let Some(entry) = reader.next_entry().await.map_err(|error| {
        anyhow::anyhow!(
            "could not expand workspace pattern {:?} at {:?}: {error}",
            pattern.raw,
            relative
        )
    })? {
        *visited_entries = visited_entries
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("workspace entry counter overflow"))?;
        anyhow::ensure!(
            *visited_entries <= MAX_VISITED_ENTRIES,
            "workspace expansion exceeds {MAX_VISITED_ENTRIES} filesystem entries; set Root Directory explicitly"
        );

        let name = entry.file_name();
        if PRUNED_DIRS
            .iter()
            .any(|pruned| name == std::ffi::OsStr::new(pruned))
        {
            continue;
        }
        let metadata = tokio::fs::symlink_metadata(entry.path())
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "could not inspect workspace entry {:?} for pattern {:?}: {error}",
                    entry.path(),
                    pattern.raw
                )
            })?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "workspace pattern {:?} encounters symlink {:?}; set Root Directory explicitly",
            pattern.raw,
            relative.join(&name)
        );
        if !metadata.is_dir() {
            continue;
        }
        let name = name.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "workspace directory name under {:?} is not valid UTF-8",
                relative
            )
        })?;
        let child = relative.join(name);
        let depth = child.components().count();
        anyhow::ensure!(
            depth <= MAX_MEMBER_DEPTH,
            "workspace expansion exceeds directory depth {MAX_MEMBER_DEPTH} at {:?}; set Root Directory explicitly",
            child
        );
        path_string(&child)?;
        children.push(child);
    }
    children.sort_by_key(|path| path.to_string_lossy().into_owned());
    Ok(children)
}

async fn descendants(
    root: &Path,
    relative: &Path,
    pattern: &Pattern,
    visited_entries: &mut usize,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut descendants = vec![relative.to_path_buf()];
    let mut frontier = vec![relative.to_path_buf()];
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for directory in frontier {
            next.extend(child_dirs(root, &directory, pattern, visited_entries).await?);
        }
        next.sort_by_key(|path| path.to_string_lossy().into_owned());
        next.dedup();
        descendants.extend(next.iter().cloned());
        anyhow::ensure!(
            descendants.len() <= MAX_VISITED_ENTRIES,
            "workspace recursive expansion exceeds {MAX_VISITED_ENTRIES} directories; set Root Directory explicitly"
        );
        frontier = next;
    }
    descendants.sort_by_key(|path| path.to_string_lossy().into_owned());
    descendants.dedup();
    Ok(descendants)
}

fn pattern_matches(pattern: &Pattern, path: &Path) -> anyhow::Result<bool> {
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("workspace member path is not valid UTF-8")),
            _ => Err(anyhow::anyhow!("workspace member path is not normalized")),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        components.len() <= MAX_MEMBER_DEPTH,
        "workspace member path exceeds directory depth {MAX_MEMBER_DEPTH}"
    );

    let mut matches = vec![vec![false; components.len() + 1]; pattern.segments.len() + 1];
    matches[pattern.segments.len()][components.len()] = true;
    for pattern_index in (0..pattern.segments.len()).rev() {
        for component_index in (0..=components.len()).rev() {
            matches[pattern_index][component_index] = match &pattern.segments[pattern_index] {
                Segment::Literal(literal) => {
                    component_index < components.len()
                        && literal == &components[component_index]
                        && matches[pattern_index + 1][component_index + 1]
                }
                Segment::One => {
                    component_index < components.len()
                        && matches[pattern_index + 1][component_index + 1]
                }
                Segment::Many => {
                    matches[pattern_index + 1][component_index]
                        || (component_index < components.len()
                            && matches[pattern_index][component_index + 1])
                }
            };
        }
    }
    Ok(matches[0][0])
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    let value = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("workspace member path is not valid UTF-8")),
            _ => Err(anyhow::anyhow!("workspace member path is not normalized")),
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .join("/");
    anyhow::ensure!(
        value.len() <= MAX_PATH_BYTES,
        "workspace member path exceeds {MAX_PATH_BYTES} bytes"
    );
    Ok(value)
}
