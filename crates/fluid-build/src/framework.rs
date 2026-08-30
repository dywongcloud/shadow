//! Framework detection + build planning — the front half of Framework-Defined
//! Infrastructure. Given a repo we identify the framework (à la Vercel's 35+
//! presets), then produce a [`BuildPlan`]: the install/build commands, the
//! native output directory, and which **primitive** the output maps to.

use crate::{BuildContractError, BuildContractErrorCode, OutputDirectory};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_PACKAGE_JSON_BYTES: u64 = 1024 * 1024;

/// What primitive a framework's build maps to in the Build Output API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Primitive {
    /// Pure static assets (SPA / SSG with no server).
    Static,
    /// A single serverless function fronts the app (SSR / API server).
    Serverless,
    /// Static assets + serverless/edge functions (Next.js, SvelteKit, Nuxt…).
    Hybrid,
}

#[derive(Clone, Debug, Serialize)]
pub struct FrameworkPreset {
    pub slug: &'static str,
    pub name: &'static str,
    pub install_command: &'static str,
    pub build_command: &'static str,
    pub dev_command: &'static str,
    /// Directory the framework writes its build to (relative to project root).
    pub output_dir: &'static str,
    pub primitive: Primitive,
    /// True if the framework natively emits a Build Output API (`vercel build`
    /// adapter exists), so we can parse it directly instead of synthesizing.
    pub emits_build_output: bool,
}

/// The preset catalog. A representative subset of Vercel's 35+; extend freely.
pub const PRESETS: &[FrameworkPreset] = &[
    FrameworkPreset {
        slug: "nextjs",
        name: "Next.js",
        install_command: "npm install",
        build_command: "next build",
        dev_command: "next dev",
        output_dir: ".next",
        primitive: Primitive::Hybrid,
        emits_build_output: true,
    },
    // OpenNext: builds a Next.js app into `.open-next/` (server functions +
    // `assets/`). We run it with the `node` wrapper so the server function is a
    // standalone HTTP server on $PORT (see git.rs), giving it Fluid compute.
    FrameworkPreset {
        slug: "opennext",
        name: "OpenNext",
        install_command: "npm install",
        build_command: "open-next build",
        dev_command: "next dev",
        output_dir: ".open-next",
        primitive: Primitive::Hybrid,
        emits_build_output: false,
    },
    // vinext: Cloudflare's Vite reimplementation of the Next.js API. `vinext build`
    // emits a Nitro `.output/` (server + public); `vinext start` serves it on $PORT.
    FrameworkPreset {
        slug: "vinext",
        name: "vinext",
        install_command: "npm install",
        build_command: "vinext build",
        dev_command: "vinext dev",
        output_dir: ".output",
        primitive: Primitive::Hybrid,
        emits_build_output: false,
    },
    FrameworkPreset {
        slug: "nuxtjs",
        name: "Nuxt",
        install_command: "npm install",
        build_command: "nuxt build",
        dev_command: "nuxt dev",
        output_dir: ".output",
        primitive: Primitive::Hybrid,
        emits_build_output: true,
    },
    FrameworkPreset {
        slug: "sveltekit",
        name: "SvelteKit",
        install_command: "npm install",
        build_command: "vite build",
        dev_command: "vite dev",
        output_dir: ".svelte-kit",
        primitive: Primitive::Hybrid,
        emits_build_output: true,
    },
    FrameworkPreset {
        slug: "remix",
        name: "Remix",
        install_command: "npm install",
        build_command: "remix vite:build",
        dev_command: "remix vite:dev",
        output_dir: "build",
        primitive: Primitive::Hybrid,
        emits_build_output: true,
    },
    FrameworkPreset {
        slug: "astro",
        name: "Astro",
        install_command: "npm install",
        build_command: "astro build",
        dev_command: "astro dev",
        output_dir: "dist",
        primitive: Primitive::Hybrid,
        emits_build_output: false,
    },
    FrameworkPreset {
        slug: "gatsby",
        name: "Gatsby",
        install_command: "npm install",
        build_command: "gatsby build",
        dev_command: "gatsby develop",
        output_dir: "public",
        primitive: Primitive::Static,
        emits_build_output: false,
    },
    FrameworkPreset {
        slug: "vite",
        name: "Vite",
        install_command: "npm install",
        build_command: "vite build",
        dev_command: "vite",
        output_dir: "dist",
        primitive: Primitive::Static,
        emits_build_output: false,
    },
    FrameworkPreset {
        slug: "create-react-app",
        name: "Create React App",
        install_command: "npm install",
        build_command: "react-scripts build",
        dev_command: "react-scripts start",
        output_dir: "build",
        primitive: Primitive::Static,
        emits_build_output: false,
    },
    FrameworkPreset {
        slug: "vue",
        name: "Vue",
        install_command: "npm install",
        build_command: "vue-cli-service build",
        dev_command: "vue-cli-service serve",
        output_dir: "dist",
        primitive: Primitive::Static,
        emits_build_output: false,
    },
    FrameworkPreset {
        slug: "node",
        name: "Node.js Server",
        install_command: "npm install",
        build_command: "npm run build",
        dev_command: "npm start",
        output_dir: ".",
        primitive: Primitive::Serverless,
        emits_build_output: false,
    },
    FrameworkPreset {
        slug: "static",
        name: "Static",
        install_command: "",
        build_command: "",
        dev_command: "",
        output_dir: ".",
        primitive: Primitive::Static,
        emits_build_output: false,
    },
];

pub fn preset(slug: &str) -> Option<&'static FrameworkPreset> {
    PRESETS.iter().find(|p| p.slug == slug)
}

/// Where the selected package manager came from — surfaced in build logs and
/// deployment metadata so a conflicting-lockfile situation is auditable, never
/// a silent guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageManagerSource {
    /// A valid, exact `package.json#packageManager` declaration.
    PackageJson,
    /// Legacy name retained until protected build-executor callers migrate to
    /// `PackageJson`; new detection never emits it.
    Corepack,
    BunLock,
    PnpmLock,
    YarnLock,
    NpmLock,
    PnpmWorkspace,
    /// No signal at all — the platform default.
    Default,
    /// Compatibility sentinel for callers of the legacy infallible detector.
    InvalidDeclaration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageManagerDeclaration {
    /// Byte-for-byte package.json declaration. npm/pnpm/Yarn declarations are
    /// safe for Corepack after validation; Bun remains a native pinned-builder
    /// selection and may not carry a Corepack integrity suffix.
    pub raw: String,
    pub version: String,
    pub integrity: Option<String>,
}

/// Full package-manager detection result, including the exact root declaration
/// rather than only its manager name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageManagerLockfile {
    Bun,
    Pnpm,
    YarnClassic,
    YarnModern,
    Npm,
}

#[derive(Clone, Debug, Serialize)]
pub struct PackageManagerDetection {
    pub manager: &'static str,
    pub source: PackageManagerSource,
    pub declaration: Option<PackageManagerDeclaration>,
    pub lockfile: Option<PackageManagerLockfile>,
    pub conflict_warning: Option<String>,
    pub validation_error: Option<String>,
}

fn read_package_json_bounded(repo: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let path = repo.join("package.json");
    let path_before = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow::anyhow!("could not inspect package.json: {error}")),
    };
    anyhow::ensure!(
        path_before.file_type().is_file() && !path_before.file_type().is_symlink(),
        "package.json must be a regular file, not a symlink or directory"
    );
    anyhow::ensure!(
        path_before.len() <= MAX_PACKAGE_JSON_BYTES,
        "package.json is {} bytes; limit is {MAX_PACKAGE_JSON_BYTES}",
        path_before.len()
    );

    let mut file = File::open(&path)
        .map_err(|error| anyhow::anyhow!("could not open package.json: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| anyhow::anyhow!("could not inspect open package.json: {error}"))?;
    anyhow::ensure!(opened.is_file(), "package.json must be a regular file");
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.by_ref()
        .take(MAX_PACKAGE_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow::anyhow!("could not read package.json: {error}"))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_PACKAGE_JSON_BYTES,
        "package.json grew beyond {MAX_PACKAGE_JSON_BYTES} bytes while it was read"
    );
    let path_after = std::fs::symlink_metadata(&path)
        .map_err(|error| anyhow::anyhow!("could not re-inspect package.json: {error}"))?;
    anyhow::ensure!(
        path_after.file_type().is_file() && !path_after.file_type().is_symlink(),
        "package.json changed into a symlink or non-file while it was read"
    );
    anyhow::ensure!(
        opened.len() == path_after.len() && opened.modified().ok() == path_after.modified().ok(),
        "package.json changed while it was read; retry the build"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            path_before.dev() == opened.dev()
                && path_before.ino() == opened.ino()
                && opened.dev() == path_after.dev()
                && opened.ino() == path_after.ino(),
            "package.json identity changed while it was opened; retry the build"
        );
    }
    Ok(Some(bytes))
}

fn exact_semver(value: &str) -> bool {
    let (core, prerelease) = value
        .split_once('-')
        .map(|(core, prerelease)| (core, Some(prerelease)))
        .unwrap_or((value, None));
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return false;
    }
    prerelease.is_none_or(|prerelease| {
        !prerelease.is_empty()
            && prerelease.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && (!part.bytes().all(|byte| byte.is_ascii_digit())
                        || part.len() == 1
                        || !part.starts_with('0'))
            })
    })
}

fn parse_package_manager_declaration(
    bytes: Option<&[u8]>,
) -> anyhow::Result<Option<(&'static str, PackageManagerDeclaration)>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let package: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| anyhow::anyhow!("invalid repository package.json: {error}"))?;
    let Some(value) = package.get("packageManager") else {
        return Ok(None);
    };
    let raw = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("package.json#packageManager must be a string"))?;
    anyhow::ensure!(
        !raw.is_empty()
            && raw.len() <= 512
            && raw.is_ascii()
            && raw
                .bytes()
                .all(|byte| !byte.is_ascii_whitespace() && !byte.is_ascii_control()),
        "package.json#packageManager must be a compact ASCII declaration"
    );
    let (name, release) = raw.split_once('@').ok_or_else(|| {
        anyhow::anyhow!(
            "package.json#packageManager {raw:?} is unpinned; use npm|pnpm|yarn@<exact-version>"
        )
    })?;
    anyhow::ensure!(
        !release.is_empty() && !release.contains('@'),
        "package.json#packageManager {raw:?} is malformed"
    );
    let manager = match name {
        "npm" => "npm",
        "pnpm" => "pnpm",
        "yarn" => "yarn",
        "bun" => "bun",
        _ => {
            anyhow::bail!("package.json#packageManager {raw:?} names unsupported manager {name:?}")
        }
    };
    let (version, integrity) = release
        .split_once('+')
        .map(|(version, integrity)| (version, Some(integrity)))
        .unwrap_or((release, None));
    anyhow::ensure!(
        exact_semver(version),
        "package.json#packageManager {raw:?} must pin an exact semantic version"
    );
    anyhow::ensure!(
        manager != "bun" || integrity.is_none(),
        "package.json#packageManager {raw:?} gives Bun a Corepack integrity suffix, which the native pinned Bun executor cannot verify"
    );
    if let Some(integrity) = integrity {
        anyhow::ensure!(
            !integrity.contains('+'),
            "package.json#packageManager {raw:?} has malformed integrity"
        );
        let (algorithm, digest) = integrity.split_once('.').ok_or_else(|| {
            anyhow::anyhow!(
                "package.json#packageManager {raw:?} integrity must be <sha-algorithm>.<hex>"
            )
        })?;
        let expected = match algorithm {
            "sha224" => 56,
            "sha256" => 64,
            "sha384" => 96,
            "sha512" => 128,
            _ => anyhow::bail!(
                "package.json#packageManager {raw:?} uses unsupported integrity algorithm {algorithm:?}"
            ),
        };
        anyhow::ensure!(
            digest.len() == expected
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "package.json#packageManager {raw:?} has an invalid {algorithm} digest"
        );
    }
    Ok(Some((
        manager,
        PackageManagerDeclaration {
            raw: raw.to_string(),
            version: version.to_string(),
            integrity: integrity.map(str::to_string),
        },
    )))
}

fn regular_signal(repo: &Path, name: &str) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(repo.join(name)) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "package-manager marker {name} must be a regular file"
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(anyhow::anyhow!("could not inspect {name}: {error}")),
    }
}

fn yarn_lockfile_kind(repo: &Path) -> anyhow::Result<PackageManagerLockfile> {
    const MAX_YARN_LOCK_HEADER_BYTES: u64 = 64 * 1024;
    let mut file = File::open(repo.join("yarn.lock"))
        .map_err(|error| anyhow::anyhow!("could not open yarn.lock: {error}"))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_YARN_LOCK_HEADER_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow::anyhow!("could not read yarn.lock: {error}"))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| anyhow::anyhow!("yarn.lock is not valid UTF-8"))?;
    if text.lines().any(|line| line.trim() == "__metadata:") {
        return Ok(PackageManagerLockfile::YarnModern);
    }
    if text
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("# yarn lockfile v1"))
    {
        return Ok(PackageManagerLockfile::YarnClassic);
    }
    anyhow::bail!(
        "yarn.lock format is ambiguous; declare package.json#packageManager with an exact Yarn version"
    )
}

pub fn detect_package_manager_checked(repo: &Path) -> anyhow::Result<PackageManagerDetection> {
    let package_bytes = read_package_json_bounded(repo)?;
    let declaration = parse_package_manager_declaration(package_bytes.as_deref())?;
    let bun_lock = regular_signal(repo, "bun.lock")? || regular_signal(repo, "bun.lockb")?;
    let pnpm_lock = regular_signal(repo, "pnpm-lock.yaml")?;
    let yarn_lock = regular_signal(repo, "yarn.lock")?;
    let npm_lock = regular_signal(repo, "package-lock.json")?;
    let pnpm_workspace = regular_signal(repo, "pnpm-workspace.yaml")?;

    let (manager, source, declaration) = declaration
        .map(|(manager, declaration)| {
            (
                manager,
                PackageManagerSource::PackageJson,
                Some(declaration),
            )
        })
        .or_else(|| bun_lock.then_some(("bun", PackageManagerSource::BunLock, None)))
        .or_else(|| pnpm_lock.then_some(("pnpm", PackageManagerSource::PnpmLock, None)))
        .or_else(|| yarn_lock.then_some(("yarn", PackageManagerSource::YarnLock, None)))
        .or_else(|| npm_lock.then_some(("npm", PackageManagerSource::NpmLock, None)))
        .unwrap_or(("npm", PackageManagerSource::Default, None));

    let mut conflicting = Vec::new();
    if bun_lock && manager != "bun" {
        conflicting.push("bun.lock/bun.lockb");
    }
    if pnpm_lock && manager != "pnpm" {
        conflicting.push("pnpm-lock.yaml");
    }
    if yarn_lock && manager != "yarn" {
        conflicting.push("yarn.lock");
    }
    if npm_lock && manager != "npm" {
        conflicting.push("package-lock.json");
    }
    if pnpm_workspace && manager != "pnpm" {
        conflicting.push("pnpm-workspace.yaml (non-authoritative workspace metadata)");
    }
    let conflict_warning = (!conflicting.is_empty()).then(|| {
        let selector = match source {
            PackageManagerSource::PackageJson => {
                format!("package.json#packageManager selects \"{manager}\" exactly")
            }
            _ => format!("lockfile precedence (bun > pnpm > yarn > npm) selects \"{manager}\""),
        };
        format!(
            "{selector}; ignoring stale conflicting lockfile(s): {}.",
            conflicting.join(", ")
        )
    });

    let lockfile = match manager {
        "bun" if bun_lock => Some(PackageManagerLockfile::Bun),
        "pnpm" if pnpm_lock => Some(PackageManagerLockfile::Pnpm),
        "yarn" if yarn_lock => {
            let kind = if let Some(declaration) = declaration.as_ref() {
                let major = declaration
                    .version
                    .split('.')
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not read Yarn major from packageManager {:?}",
                            declaration.raw
                        )
                    })?;
                if major == 1 {
                    PackageManagerLockfile::YarnClassic
                } else {
                    PackageManagerLockfile::YarnModern
                }
            } else {
                yarn_lockfile_kind(repo)?
            };
            Some(kind)
        }
        "npm" if npm_lock => Some(PackageManagerLockfile::Npm),
        _ => None,
    };

    Ok(PackageManagerDetection {
        manager,
        source,
        declaration,
        lockfile,
        conflict_warning,
        validation_error: None,
    })
}

/// Compatibility surface for existing callers. Invalid declarations never
/// fall back to another manager: the sentinel plus diagnostic makes the current
/// build path fail before it can execute an install command.
pub fn detect_package_manager(repo: &Path) -> PackageManagerDetection {
    detect_package_manager_checked(repo).unwrap_or_else(|error| {
        let message = error.to_string();
        PackageManagerDetection {
            manager: "invalid",
            source: PackageManagerSource::InvalidDeclaration,
            declaration: None,
            lockfile: None,
            conflict_warning: Some(message.clone()),
            validation_error: Some(message),
        }
    })
}

/// Detect the JS package manager from `package.json#packageManager` (Corepack)
/// and lockfiles (Vercel's precedence: bun -> pnpm -> yarn -> npm). Defaults to
/// npm. Thin wrapper over [`detect_package_manager`] for callers that only need
/// the manager name, not the full provenance/conflict diagnostics.
pub fn package_manager(repo: &Path) -> &'static str {
    detect_package_manager(repo).manager
}

/// The install command for a package-manager plan. Exact package.json pins use
/// the declaration verbatim; lockfile/default selections use the builder's
/// pinned manager binary.
fn install_for(detection: &PackageManagerDetection) -> String {
    let arguments = match (detection.manager, detection.lockfile) {
        ("bun", Some(PackageManagerLockfile::Bun)) => "install --frozen-lockfile",
        ("pnpm", Some(PackageManagerLockfile::Pnpm)) => "install --frozen-lockfile",
        ("yarn", Some(PackageManagerLockfile::YarnClassic)) => "install --frozen-lockfile",
        ("yarn", Some(PackageManagerLockfile::YarnModern)) => "install --immutable",
        _ => "install",
    };
    if let Some(declaration) = &detection.declaration {
        if detection.manager == "bun" {
            return format!("bun {arguments}");
        }
        return format!("corepack {} {arguments}", declaration.raw);
    }
    format!("{} {arguments}", detection.manager)
}

fn manager_command(detection: &PackageManagerDetection) -> String {
    match &detection.declaration {
        Some(declaration) if detection.manager != "bun" => {
            format!("corepack {}", declaration.raw)
        }
        _ => detection.manager.to_string(),
    }
}

/// Rewrite only framework-provided `npm …` defaults. Explicit overrides never
/// pass through this function.
fn pmify(cmd: &str, detection: &PackageManagerDetection) -> String {
    if detection.manager == "npm" && detection.declaration.is_none() {
        return cmd.to_string();
    }
    let manager = manager_command(detection);
    let c = cmd.trim();
    if let Some(rest) = c.strip_prefix("npm run ") {
        return if detection.manager == "yarn" {
            format!("{manager} {rest}")
        } else {
            format!("{manager} run {rest}")
        };
    }
    if c == "npm install" || c == "npm i" {
        return install_for(detection);
    }
    if let Some(rest) = c.strip_prefix("npm exec ") {
        return format!("{manager} exec {rest}");
    }
    cmd.to_string()
}

/// Concrete plan for building one repo.
#[derive(Clone, Debug, Serialize)]
pub struct BuildPlan {
    pub framework: FrameworkPreset,
    /// Detected package manager: npm | yarn | pnpm | bun | invalid.
    pub package_manager: String,
    pub package_manager_declaration: Option<PackageManagerDeclaration>,
    pub package_manager_error: Option<String>,
    pub install_command: String,
    pub build_command: String,
    pub output_dir: OutputDirectory,
}

/// Detect the framework for a repo by inspecting marker files + package.json
/// dependencies. Order matters: most specific first.
pub fn detect_checked(repo: &Path) -> Result<&'static FrameworkPreset, BuildContractError> {
    let package_bytes = read_package_json_bounded(repo)
        .map_err(|error| BuildContractError::invalid_metadata("detect framework", error))?;
    let package = package_bytes
        .as_deref()
        .map(serde_json::from_slice::<serde_json::Value>)
        .transpose()
        .map_err(|error| {
            BuildContractError::invalid_metadata(
                "detect framework",
                format!("invalid repository package.json: {error}"),
            )
        })?;
    Ok(detect_with_package(repo, package.as_ref()))
}

pub fn detect(repo: &Path) -> &'static FrameworkPreset {
    let package = std::fs::read_to_string(repo.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    detect_with_package(repo, package.as_ref())
}

fn detect_with_package(
    repo: &Path,
    package: Option<&serde_json::Value>,
) -> &'static FrameworkPreset {
    // 0) Next.js DEPLOYMENT ADAPTERS take precedence — an OpenNext/vinext project
    //    still has `next` as a dependency and a `next.config.*`, so they'd
    //    otherwise be misdetected as plain Next.js. Check their marker deps/files
    //    first. `open-next.config.*` is unique to OpenNext; `vinext` is a dep.
    for f in [
        "open-next.config.ts",
        "open-next.config.js",
        "open-next.config.mjs",
    ] {
        if repo.join(f).exists() {
            return preset("opennext").unwrap();
        }
    }
    if let Some(package) = package {
        if has_dep(package, "@opennextjs/aws") || has_dep(package, "open-next") {
            return preset("opennext").unwrap();
        }
        if has_dep(package, "vinext") {
            return preset("vinext").unwrap();
        }
    }

    // 1) Config-file markers (cheapest, most reliable).
    let markers: &[(&str, &str)] = &[
        ("next.config.js", "nextjs"),
        ("next.config.mjs", "nextjs"),
        ("next.config.ts", "nextjs"),
        ("nuxt.config.js", "nuxtjs"),
        ("nuxt.config.ts", "nuxtjs"),
        ("svelte.config.js", "sveltekit"),
        ("astro.config.mjs", "astro"),
        ("astro.config.js", "astro"),
        ("astro.config.ts", "astro"),
        ("gatsby-config.js", "gatsby"),
        ("gatsby-config.ts", "gatsby"),
        ("remix.config.js", "remix"),
        ("vite.config.js", "vite"),
        ("vite.config.ts", "vite"),
        ("vue.config.js", "vue"),
    ];
    for (file, slug) in markers {
        if repo.join(file).exists() {
            if let Some(preset) = preset(slug) {
                return preset;
            }
        }
    }

    // 2) package.json dependency sniffing.
    if let Some(package) = package {
        let dep = |name: &str| has_dep(package, name);
        if dep("next") {
            return preset("nextjs").unwrap();
        }
        if dep("nuxt") || dep("nuxt3") {
            return preset("nuxtjs").unwrap();
        }
        if dep("@sveltejs/kit") {
            return preset("sveltekit").unwrap();
        }
        if dep("@remix-run/dev") {
            return preset("remix").unwrap();
        }
        if dep("astro") {
            return preset("astro").unwrap();
        }
        if dep("gatsby") {
            return preset("gatsby").unwrap();
        }
        if dep("react-scripts") {
            return preset("create-react-app").unwrap();
        }
        if dep("vite") {
            return preset("vite").unwrap();
        }
        if dep("@vue/cli-service") {
            return preset("vue").unwrap();
        }
        // A Node server FRAMEWORK dependency => a server app even with no
        // start script. Without this, a repo like vercel/vercel's
        // examples/express (dependency `express`, no scripts at all) fell all
        // the way through to the static preset and shipped its raw source tree
        // as a website — a silently wrong deployment the readiness probe then
        // refused with an unrelated-looking error. Detecting the server intent
        // routes it to the Node lane, where a missing runnable entry fails
        // LOUDLY with the existing "no usable production server entry" gate
        // naming the real remedy (a start script) instead.
        for server_dep in ["express", "fastify", "koa", "@nestjs/core", "hono"] {
            if dep(server_dep) {
                return preset("node").unwrap();
            }
        }
        // A server start script => treat as a Node serverless app.
        if package
            .get("scripts")
            .and_then(|scripts| scripts.get("start"))
            .is_some()
        {
            return preset("node").unwrap();
        }
    }

    // 3) Plain static site (has an index.html) or unknown -> static.
    preset("static").unwrap()
}

fn has_dep(pkg: &serde_json::Value, name: &str) -> bool {
    for key in ["dependencies", "devDependencies"] {
        if pkg.get(key).and_then(|d| d.get(name)).is_some() {
            return true;
        }
    }
    false
}

/// Produce the build plan, honoring overrides the user set in project settings
/// (empty string => use the framework default).
/// Find a preset by slug or display name (case-insensitive), so an explicit
/// framework choice in project settings can override auto-detection.
pub fn preset_by_name(name: &str) -> Option<&'static FrameworkPreset> {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() {
        return None;
    }
    let n = match n.as_str() {
        // Dashboard display label retained for compatibility with settings rows
        // written before it used canonical preset slugs.
        "node (express)" => "node",
        _ => n.as_str(),
    };
    PRESETS
        .iter()
        .find(|p| p.slug.eq_ignore_ascii_case(n) || p.name.eq_ignore_ascii_case(n))
}

pub fn plan_build(
    repo: &Path,
    framework_override: Option<&str>,
    install_override: Option<&str>,
    build_override: Option<&str>,
    output_override: Option<&str>,
) -> BuildPlan {
    let detection = detect_package_manager(repo);
    plan_build_with_package_manager(
        repo,
        &detection,
        framework_override,
        install_override,
        build_override,
        output_override,
    )
}

/// Produce the one fail-closed build plan used by production build admission.
/// Package-manager inputs come from the caller's authoritative install root;
/// framework and output settings are resolved at the selected application.
pub fn plan_build_checked_with_package_manager(
    repo: &Path,
    detection: &PackageManagerDetection,
    framework_override: Option<&str>,
    install_override: Option<&str>,
    build_override: Option<&str>,
    output_override: Option<&str>,
) -> Result<BuildPlan, BuildContractError> {
    if let Some(error) = &detection.validation_error {
        return Err(BuildContractError::invalid_metadata(
            "resolve package manager",
            error,
        ));
    }
    let framework_override = framework_override
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let framework = match framework_override {
        Some(value) => preset_by_name(value).cloned().ok_or_else(|| {
            BuildContractError::new(
                BuildContractErrorCode::InvalidFramework,
                "resolve explicit framework",
                format!("unknown explicit framework {value:?}"),
            )
        })?,
        None => detect_checked(repo)?.clone(),
    };
    let command_pick = |override_value: Option<&str>, default: String| {
        override_value
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or(default)
    };
    let output = output_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(framework.output_dir);
    let output_dir = OutputDirectory::parse(output)?;
    let install_default = install_for(detection);
    let build_default = pmify(framework.build_command, detection);
    Ok(BuildPlan {
        install_command: command_pick(install_override, install_default),
        build_command: command_pick(build_override, build_default),
        output_dir,
        package_manager: detection.manager.to_string(),
        package_manager_declaration: detection.declaration.clone(),
        package_manager_error: None,
        framework,
    })
}

/// Compatibility surface for read-only callers. Production uses
/// [`plan_build_checked_with_package_manager`]; an invalid plan stays visibly
/// invalid and receives only a non-escaping placeholder path that is never run.
pub fn plan_build_with_package_manager(
    repo: &Path,
    detection: &PackageManagerDetection,
    framework_override: Option<&str>,
    install_override: Option<&str>,
    build_override: Option<&str>,
    output_override: Option<&str>,
) -> BuildPlan {
    match plan_build_checked_with_package_manager(
        repo,
        detection,
        framework_override,
        install_override,
        build_override,
        output_override,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            let framework = framework_override
                .and_then(preset_by_name)
                .cloned()
                .unwrap_or_else(|| detect(repo).clone());
            let command_pick = |override_value: Option<&str>, default: String| {
                override_value
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or(default)
            };
            BuildPlan {
                install_command: command_pick(install_override, install_for(detection)),
                build_command: command_pick(
                    build_override,
                    pmify(framework.build_command, detection),
                ),
                output_dir: OutputDirectory::root(),
                package_manager: detection.manager.to_string(),
                package_manager_declaration: detection.declaration.clone(),
                package_manager_error: Some(error.to_string()),
                framework,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo_with(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let id = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("fw-{}-{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            let p = dir.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        dir
    }

    #[test]
    fn detects_nextjs_from_dependency() {
        let dir = repo_with(&[(
            "package.json",
            r#"{"dependencies":{"next":"14.0.0","react":"18"}}"#,
        )]);
        let p = detect(&dir);
        assert_eq!(p.slug, "nextjs");
        assert_eq!(p.primitive, Primitive::Hybrid);
        assert!(p.emits_build_output);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_opennext_over_nextjs() {
        // An OpenNext project has `next` + `next.config.js` too — adapter must win.
        let dir = repo_with(&[
            ("next.config.js", "module.exports = {}"),
            ("open-next.config.ts", "export default {}"),
            (
                "package.json",
                r#"{"dependencies":{"next":"14","@opennextjs/aws":"3"}}"#,
            ),
        ]);
        let p = detect(&dir);
        assert_eq!(p.slug, "opennext");
        assert_eq!(p.output_dir, ".open-next");
        assert_eq!(p.primitive, Primitive::Hybrid);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_opennext_from_dep_only() {
        let dir = repo_with(&[
            ("next.config.js", "module.exports = {}"),
            (
                "package.json",
                r#"{"dependencies":{"next":"14","open-next":"2"}}"#,
            ),
        ]);
        assert_eq!(detect(&dir).slug, "opennext");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_vinext_over_nextjs() {
        // vinext also carries next.config + a `next` dep; the `vinext` dep wins.
        let dir = repo_with(&[
            ("next.config.js", "module.exports = {}"),
            (
                "package.json",
                r#"{"dependencies":{"next":"16","vinext":"0.1"}}"#,
            ),
        ]);
        let p = detect(&dir);
        assert_eq!(p.slug, "vinext");
        assert_eq!(p.output_dir, ".output");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_vite_static() {
        let dir = repo_with(&[
            ("vite.config.ts", "export default {}"),
            ("package.json", "{}"),
        ]);
        assert_eq!(detect(&dir).slug, "vite");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_static() {
        let dir = repo_with(&[("index.html", "<!doctype html>")]);
        assert_eq!(detect(&dir).slug, "static");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_override_wins() {
        let dir = repo_with(&[("package.json", r#"{"dependencies":{"next":"14"}}"#)]);
        let plan = plan_build(&dir, None, None, Some("pnpm build"), None);
        assert_eq!(plan.build_command, "pnpm build");
        assert_eq!(plan.framework.slug, "nextjs");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_manager_precedence_matches_vercel_lockfile_order() {
        let dir = repo_with(&[("bun.lock", "")]);
        assert_eq!(package_manager(&dir), "bun");
        let _ = fs::remove_dir_all(&dir);

        let dir = repo_with(&[("bun.lockb", "")]);
        assert_eq!(package_manager(&dir), "bun");
        let _ = fs::remove_dir_all(&dir);

        let dir = repo_with(&[("pnpm-lock.yaml", "")]);
        assert_eq!(package_manager(&dir), "pnpm");
        let _ = fs::remove_dir_all(&dir);

        let dir = repo_with(&[("yarn.lock", "# yarn lockfile v1\n")]);
        assert_eq!(package_manager(&dir), "yarn");
        let _ = fs::remove_dir_all(&dir);

        let dir = repo_with(&[("package-lock.json", "{}")]);
        assert_eq!(package_manager(&dir), "npm");
        let _ = fs::remove_dir_all(&dir);

        let dir = repo_with(&[]);
        assert_eq!(package_manager(&dir), "npm");
        let d = detect_package_manager(&dir);
        assert_eq!(d.source, PackageManagerSource::Default);
        assert!(d.conflict_warning.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corepack_package_manager_field_wins_over_every_lockfile() {
        // packageManager (Corepack) says pnpm; a yarn.lock is ALSO present —
        // Corepack must win, and the yarn.lock must be reported as a conflict,
        // never silently deleted or ignored.
        let dir = repo_with(&[
            ("package.json", r#"{"packageManager":"pnpm@8.15.4"}"#),
            ("yarn.lock", ""),
        ]);
        let d = detect_package_manager(&dir);
        assert_eq!(d.manager, "pnpm");
        assert_eq!(d.source, PackageManagerSource::PackageJson);
        let warning = d
            .conflict_warning
            .expect("must warn about the conflicting yarn.lock");
        assert!(
            warning.contains("yarn.lock"),
            "warning should name the conflicting file: {warning}"
        );
        assert!(
            std::path::Path::new(&dir).join("yarn.lock").exists(),
            "must never delete the conflicting lockfile"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corepack_package_manager_field_selects_bun_runtime_agnostic() {
        let dir = repo_with(&[("package.json", r#"{"packageManager":"bun@1.2.3"}"#)]);
        let d = detect_package_manager(&dir);
        assert_eq!(d.manager, "bun");
        assert_eq!(d.source, PackageManagerSource::PackageJson);
        assert!(d.conflict_warning.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn conflicting_lockfiles_without_declaration_resolve_by_precedence() {
        // No packageManager field and TWO lockfiles — the documented Vercel
        // precedence (bun > pnpm > yarn > npm) picks the winner and the loser
        // is surfaced in the conflict warning, never a hard refusal: a stale
        // second lockfile is a routine migration leftover, and refusing it
        // failed every such repo's deploy outright (witnessed live 2026-08-23).
        let dir = repo_with(&[("bun.lock", ""), ("pnpm-lock.yaml", "")]);
        let d = detect_package_manager(&dir);
        assert_eq!(d.manager, "bun");
        assert_eq!(d.source, PackageManagerSource::BunLock);
        let warning = d
            .conflict_warning
            .expect("must name the conflicting lockfile");
        assert!(warning.contains("pnpm-lock.yaml"), "warning: {warning}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_or_unrecognized_package_manager_field_refuses_loudly() {
        // Unparseable JSON → loud refusal, never a silent lockfile fallback
        // (and never a panic).
        let dir = repo_with(&[("package.json", "{not json"), ("yarn.lock", "")]);
        let d = detect_package_manager(&dir);
        assert_eq!(d.manager, "invalid");
        assert!(d.validation_error.is_some());
        let _ = fs::remove_dir_all(&dir);

        // Recognized JSON but an unknown manager name → same loud refusal.
        let dir = repo_with(&[
            ("package.json", r#"{"packageManager":"deno@1.0.0"}"#),
            ("yarn.lock", ""),
        ]);
        let d = detect_package_manager(&dir);
        assert_eq!(d.manager, "invalid");
        assert!(d.validation_error.is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
