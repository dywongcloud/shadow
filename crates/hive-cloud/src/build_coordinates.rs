use std::path::{Component, Path, PathBuf};

use fluid_core::Manifest;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckoutRoot(PathBuf);

#[derive(Clone, Debug, PartialEq, Eq)]
struct InstallRoot(PathBuf);

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedApp(PathBuf);

#[derive(Clone, Debug, PartialEq, Eq)]
struct BuildCwd(PathBuf);

#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputRoot(PathBuf);

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeArtifactBase(PathBuf);

#[derive(Clone, Debug, PartialEq, Eq)]
struct FunctionCwd(PathBuf);

/// Checked coordinate system for one selected application. Every relative role
/// is rooted at the immutable checkout; callers receive composed absolute paths
/// but cannot join one role onto another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MonorepoCoordinates {
    checkout: CheckoutRoot,
    install: InstallRoot,
    app: SelectedApp,
    build: BuildCwd,
    output: OutputRoot,
    artifact: RuntimeArtifactBase,
}

impl MonorepoCoordinates {
    pub(crate) fn new(
        checkout: &Path,
        selected_app: &Path,
        install_root: &Path,
        build_cwd: &Path,
        output_relative_to_app: &Path,
    ) -> anyhow::Result<Self> {
        let checkout = CheckoutRoot(normalized_absolute(checkout, "checkout root")?);
        let app = SelectedApp(relative_role(
            &checkout,
            selected_app,
            "selected application",
        )?);
        let install = InstallRoot(relative_role(
            &checkout,
            install_root,
            "package-manager install root",
        )?);
        let build = BuildCwd(relative_role(&checkout, build_cwd, "build cwd")?);
        let output_suffix = normalized_relative(output_relative_to_app, "output path")?;
        let output = OutputRoot(compose_relative(
            &app.0,
            &output_suffix,
            "selected application output",
        )?);
        let artifact = RuntimeArtifactBase(app.0.clone());
        Ok(Self {
            checkout,
            install,
            app,
            build,
            output,
            artifact,
        })
    }

    pub(crate) fn selected(checkout: &Path, selected_app: &Path) -> anyhow::Result<Self> {
        Self::new(
            checkout,
            selected_app,
            selected_app,
            selected_app,
            Path::new("."),
        )
    }

    pub(crate) fn checkout_root(&self) -> &Path {
        &self.checkout.0
    }

    pub(crate) fn selected_app(&self) -> PathBuf {
        self.absolute(&self.app.0)
    }

    pub(crate) fn selected_app_relative(&self) -> &Path {
        &self.app.0
    }

    pub(crate) fn install_root(&self) -> PathBuf {
        self.absolute(&self.install.0)
    }

    pub(crate) fn install_root_relative(&self) -> &Path {
        &self.install.0
    }

    pub(crate) fn build_cwd(&self) -> PathBuf {
        self.absolute(&self.build.0)
    }

    pub(crate) fn build_cwd_relative(&self) -> &Path {
        &self.build.0
    }

    pub(crate) fn output_root(&self) -> PathBuf {
        self.absolute(&self.output.0)
    }

    pub(crate) fn output_root_relative(&self) -> &Path {
        &self.output.0
    }

    pub(crate) fn output_relative_to_app(&self) -> String {
        let relative = self
            .output
            .0
            .strip_prefix(&self.app.0)
            .expect("constructor proves output is beneath selected application");
        if relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            relative.to_string_lossy().replace('\\', "/")
        }
    }

    pub(crate) fn runtime_artifact_base(&self) -> PathBuf {
        self.absolute(&self.artifact.0)
    }

    pub(crate) fn runtime_artifact_relative(&self) -> &Path {
        &self.artifact.0
    }

    pub(crate) fn is_workspace_member(&self) -> bool {
        !self.app.0.as_os_str().is_empty()
    }

    pub(crate) fn is_monorepo(&self) -> bool {
        self.is_workspace_member() && self.install.0.as_os_str().is_empty()
    }

    /// Canonicalize every function cwd as runtime-artifact-relative authority.
    /// The selected application is already represented by `artifact`; an ordinary
    /// function therefore receives `.`, never the selected path a second time.
    pub(crate) fn normalize_function_cwds(&self, manifest: &mut Manifest) -> anyhow::Result<()> {
        for function in &mut manifest.functions {
            let cwd = FunctionCwd::parse(function.cwd_relative.as_deref().unwrap_or("."))?;
            function.cwd_relative = Some(cwd.wire());
        }
        Ok(())
    }

    fn absolute(&self, relative: &Path) -> PathBuf {
        if relative.as_os_str().is_empty() {
            self.checkout.0.clone()
        } else {
            self.checkout.0.join(relative)
        }
    }
}

impl FunctionCwd {
    fn parse(value: &str) -> anyhow::Result<Self> {
        let value = value.trim();
        anyhow::ensure!(!value.is_empty(), "function cwd cannot be empty");
        Ok(Self(normalized_relative(Path::new(value), "function cwd")?))
    }

    fn wire(&self) -> String {
        if self.0.as_os_str().is_empty() {
            ".".to_string()
        } else {
            self.0.to_string_lossy().replace('\\', "/")
        }
    }
}

fn relative_role(checkout: &CheckoutRoot, absolute: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let absolute = normalized_absolute(absolute, label)?;
    let relative = absolute.strip_prefix(&checkout.0).map_err(|_| {
        anyhow::anyhow!(
            "{label} {} is outside checkout {}",
            absolute.display(),
            checkout.0.display()
        )
    })?;
    normalized_relative(relative, label)
}

fn compose_relative(base: &Path, suffix: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let base = normalized_relative(base, label)?;
    let suffix = normalized_relative(suffix, label)?;
    let composed = if base.as_os_str().is_empty() {
        suffix
    } else if suffix.as_os_str().is_empty() {
        base
    } else {
        base.join(suffix)
    };
    normalized_relative(&composed, label)
}

fn normalized_absolute(path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "{label} must be absolute: {}",
        path.display()
    );
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::Normal(name) => normalized.push(name),
            Component::CurDir | Component::ParentDir => {
                anyhow::bail!("{label} is not normalized: {}", path.display())
            }
        }
    }
    Ok(normalized)
}

fn normalized_relative(path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        !path.is_absolute(),
        "{label} must be relative: {}",
        path.display()
    );
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("{label} contains traversal: {}", path.display())
            }
        }
    }
    Ok(normalized)
}
