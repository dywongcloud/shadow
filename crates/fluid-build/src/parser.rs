//! Read a `.vercel/output` directory (Build Output API v3) into a typed
//! [`BuildOutput`] the platform can provision from.

use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context};

use crate::build_output::{
    parse_vc_config, BuildOutputConfig, FunctionConfig, PrerenderConfig, BUILD_OUTPUT_VERSION,
};

/// Input bounds are enforced while walking, before the durable descriptor is
/// built. The gateway independently re-checks its durable copy.
pub const MAX_BUILD_OUTPUT_CONFIG_BYTES: u64 = 1024 * 1024;
pub const MAX_BUILD_OUTPUT_DESCRIPTOR_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_BUILD_OUTPUT_FUNCTIONS: usize = 1024;
pub const MAX_BUILD_OUTPUT_FILES: usize = 100_000;
pub const MAX_BUILD_OUTPUT_ENTRIES: usize = 120_000;
pub const MAX_BUILD_OUTPUT_PATH_BYTES: usize = 4096;
pub const MAX_BUILD_OUTPUT_PATH_BYTES_TOTAL: usize = 8 * 1024 * 1024;
pub const MAX_BUILD_OUTPUT_PAYLOAD_BYTES_TOTAL: u64 = 16 * 1024 * 1024 * 1024;
pub const MAX_BUILD_OUTPUT_JSON_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_BUILD_OUTPUT_JSON_NODES: usize = 100_000;
pub const MAX_BUILD_OUTPUT_DEPTH: usize = 64;
const MAX_BUILD_OUTPUT_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// One function discovered under `functions/<name>.func/`.
#[derive(Clone, Debug)]
pub struct DeployedFunction {
    /// Route-relative name, e.g. `api/hello` (from `api/hello.func`).
    pub name: String,
    /// Absolute path to the `.func` directory. This is builder-local authority
    /// and is deliberately omitted from [`BuildOutput::descriptor_value`].
    pub dir: PathBuf,
    pub config: FunctionConfig,
    /// The exact parsed `.vc-config.json`, including fields this binary does not
    /// execute. Consumers must reject unsupported fields rather than losing them.
    pub raw_config: serde_json::Value,
    /// Every regular, non-symlink file below the `.func`, relative to that dir.
    pub files: Vec<String>,
    /// ISR/prerender metadata if a sibling `<name>.prerender-config.json` exists.
    pub prerender: Option<PrerenderConfig>,
    /// The exact parsed prerender config, kept beside the typed compatibility view.
    pub raw_prerender: Option<serde_json::Value>,
    /// Exact regular fallback payloads referenced by the prerender config, relative
    /// to `functions/`. They remain separate from the private `.func` inventory.
    pub prerender_files: Vec<String>,
}

/// A fully parsed Build Output.
#[derive(Clone, Debug)]
pub struct BuildOutput {
    pub config: BuildOutputConfig,
    /// The exact parsed `config.json`. `BuildOutputConfig` remains the legacy
    /// convenience view; this value is the lossless v3 conversion source.
    pub raw_config: serde_json::Value,
    pub functions: Vec<DeployedFunction>,
    /// Static asset paths, relative to `static/`.
    pub static_files: Vec<String>,
    /// Absolute path to `.vercel/output`; never serialized into a manifest.
    pub root: PathBuf,
}

impl BuildOutput {
    pub fn serverless_count(&self) -> usize {
        self.functions
            .iter()
            .filter(|f| !f.config.is_edge())
            .count()
    }

    pub fn edge_count(&self) -> usize {
        self.functions.iter().filter(|f| f.config.is_edge()).count()
    }

    pub fn has_image_optimization(&self) -> bool {
        self.config.images.is_some()
    }

    /// Crate-layering bridge into `fluid_core::BuildOutputV3`.
    ///
    /// `fluid-build` intentionally has no production dependency on `fluid-core`.
    /// The planner therefore calls
    /// `fluid_core::BuildOutputV3::from_parser_value(output.descriptor_value())`
    /// after parsing. This envelope contains no absolute path or other host
    /// authority: only exact config metadata and validated relative inventories.
    pub fn descriptor_value(&self) -> serde_json::Value {
        serde_json::json!({
            "config": self.raw_config,
            "functions": self.functions.iter().map(|function| serde_json::json!({
                "name": function.name,
                "config": function.raw_config,
                "prerender": function.raw_prerender,
                "files": function.files,
                "prerender_files": function.prerender_files,
            })).collect::<Vec<_>>(),
            "assets": self.static_files,
        })
    }
}

/// True when `repo` contains anything claiming the Build Output location.
///
/// Detection is intentionally broader than validity. A symlink, special path,
/// directory with no config, or unreadable candidate must reach
/// [`parse_build_output`] and fail loudly; returning false would let a caller
/// degrade malformed v3 input to a static fallback.
pub fn has_build_output(repo: &Path) -> bool {
    match std::fs::symlink_metadata(repo.join(".vercel/output")) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Parse `<repo>/.vercel/output` into a [`BuildOutput`].
pub fn parse_build_output(repo: &Path) -> anyhow::Result<BuildOutput> {
    let vercel = repo.join(".vercel");
    require_exact_dir(&vercel, ".vercel")?;
    let root = vercel.join("output");
    require_exact_dir(&root, ".vercel/output")?;
    reject_unsupported_primitive(&root.join("immutable.json"), "immutable static manifest")?;

    let mut budget = ParseBudget::default();
    budget.record_file("config.json")?;
    let config_path = root.join("config.json");
    let raw_config = read_json_regular(
        &config_path,
        "config.json",
        MAX_BUILD_OUTPUT_CONFIG_BYTES,
        &mut budget,
        false,
    )?;
    let config: BuildOutputConfig = serde_json::from_value(raw_config.clone())
        .context("BUILD_OUTPUT_V3_INVALID: config.json does not match v3")?;
    if config.version != BUILD_OUTPUT_VERSION {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: config.json version is {}, expected exactly {}",
            config.version,
            BUILD_OUTPUT_VERSION
        );
    }

    let functions = parse_functions(&root.join("functions"), &mut budget)?;
    let static_files = list_static(&root.join("static"), &mut budget)?;
    let output = BuildOutput {
        config,
        raw_config,
        functions,
        static_files,
        root,
    };
    let descriptor_bytes = serde_json::to_vec(&output.descriptor_value())
        .context("BUILD_OUTPUT_V3_INVALID: descriptor serialization failed")?
        .len();
    if descriptor_bytes > MAX_BUILD_OUTPUT_DESCRIPTOR_BYTES {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: descriptor is {descriptor_bytes} bytes, over the {MAX_BUILD_OUTPUT_DESCRIPTOR_BYTES}-byte bound"
        );
    }
    Ok(output)
}

#[derive(Default)]
struct ParseBudget {
    entries: usize,
    files: usize,
    path_bytes: usize,
    metadata_bytes: usize,
    payload_bytes: u64,
    json_nodes: usize,
}

impl ParseBudget {
    fn record_entry(&mut self, relative: &str) -> anyhow::Result<()> {
        if relative.is_empty() || relative.len() > MAX_BUILD_OUTPUT_PATH_BYTES {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: path length {} is outside 1..={MAX_BUILD_OUTPUT_PATH_BYTES}",
                relative.len()
            );
        }
        self.entries = self.entries.saturating_add(1);
        if self.entries > MAX_BUILD_OUTPUT_ENTRIES {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: output contains more than {MAX_BUILD_OUTPUT_ENTRIES} filesystem entries"
            );
        }
        self.path_bytes = self.path_bytes.saturating_add(relative.len());
        if self.path_bytes > MAX_BUILD_OUTPUT_PATH_BYTES_TOTAL {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: aggregate relative paths exceed {MAX_BUILD_OUTPUT_PATH_BYTES_TOTAL} bytes"
            );
        }
        Ok(())
    }

    fn record_dir(&mut self, relative: &str) -> anyhow::Result<()> {
        self.record_entry(relative)
    }

    fn record_file(&mut self, relative: &str) -> anyhow::Result<()> {
        self.record_entry(relative)?;
        self.files = self.files.saturating_add(1);
        if self.files > MAX_BUILD_OUTPUT_FILES {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: output contains more than {MAX_BUILD_OUTPUT_FILES} files"
            );
        }
        Ok(())
    }

    fn record_metadata(&mut self, bytes: usize) -> anyhow::Result<()> {
        self.metadata_bytes = self.metadata_bytes.saturating_add(bytes);
        if self.metadata_bytes > MAX_BUILD_OUTPUT_DESCRIPTOR_BYTES {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: config metadata exceeds {MAX_BUILD_OUTPUT_DESCRIPTOR_BYTES} bytes"
            );
        }
        Ok(())
    }

    fn record_payload(&mut self, bytes: u64) -> anyhow::Result<()> {
        self.payload_bytes = self.payload_bytes.saturating_add(bytes);
        if self.payload_bytes > MAX_BUILD_OUTPUT_PAYLOAD_BYTES_TOTAL {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: aggregate output payload exceeds {MAX_BUILD_OUTPUT_PAYLOAD_BYTES_TOTAL} bytes"
            );
        }
        Ok(())
    }
}

fn reject_unsupported_primitive(path: &Path, feature: &str) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "BUILD_OUTPUT_V3_CAPABILITY_UNSUPPORTED: {feature} at {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "BUILD_OUTPUT_V3_INVALID: cannot inspect optional {feature} at {}",
                path.display()
            )
        }),
    }
}

fn require_exact_dir(path: &Path, label: &str) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect {label}"))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        bail!("BUILD_OUTPUT_V3_INVALID: {label} must be a regular, non-symlink directory");
    }
    Ok(())
}

fn optional_exact_dir(path: &Path, label: &str) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() || !file_type.is_dir() {
                bail!("BUILD_OUTPUT_V3_INVALID: {label} must be a regular, non-symlink directory");
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect {label}"))
        }
    }
}

fn read_json_regular(
    path: &Path,
    label: &str,
    max_bytes: u64,
    budget: &mut ParseBudget,
    payload_already_recorded: bool,
) -> anyhow::Result<serde_json::Value> {
    let linked = std::fs::symlink_metadata(path)
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect {label}"))?;
    if linked.file_type().is_symlink() || !linked.file_type().is_file() {
        bail!("BUILD_OUTPUT_V3_INVALID: {label} must be a regular, non-symlink file");
    }
    if linked.len() > max_bytes {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: {label} is {} bytes, over the {max_bytes}-byte bound",
            linked.len()
        );
    }
    if !payload_already_recorded {
        budget.record_payload(linked.len())?;
    }

    let mut file = File::open(path)
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot open {label}"))?;
    let opened = file
        .metadata()
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect open {label}"))?;
    if !opened.is_file() || !same_file(&linked, &opened) {
        bail!("BUILD_OUTPUT_V3_INVALID: {label} changed identity while opening");
    }
    let expected = opened.len() as usize;
    let mut bytes = Vec::with_capacity(expected);
    (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot read {label}"))?;
    let after = file
        .metadata()
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot re-inspect {label}"))?;
    if bytes.len() != expected
        || !same_file(&opened, &after)
        || opened.len() != after.len()
        || opened.modified().ok() != after.modified().ok()
    {
        bail!("BUILD_OUTPUT_V3_INVALID: {label} changed while reading");
    }
    budget.record_metadata(bytes.len())?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: malformed {label}"))?;
    validate_json(&value, label, 0, budget)?;
    Ok(value)
}

#[cfg(unix)]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn inspect_regular_file(path: &Path, label: &str, max_bytes: u64) -> anyhow::Result<u64> {
    let linked = std::fs::symlink_metadata(path)
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect {label}"))?;
    if linked.file_type().is_symlink() || !linked.file_type().is_file() {
        bail!("BUILD_OUTPUT_V3_INVALID: {label} must be a regular, non-symlink file");
    }
    if linked.len() > max_bytes {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: {label} is {} bytes, over the {max_bytes}-byte bound",
            linked.len()
        );
    }
    let file = File::open(path)
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot open {label}"))?;
    let opened = file
        .metadata()
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect open {label}"))?;
    let after = file
        .metadata()
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot re-inspect {label}"))?;
    if !opened.is_file()
        || !same_file(&linked, &opened)
        || !same_file(&opened, &after)
        || opened.len() != after.len()
        || opened.modified().ok() != after.modified().ok()
    {
        bail!("BUILD_OUTPUT_V3_INVALID: {label} changed identity while inspecting");
    }
    Ok(opened.len())
}

fn validate_json(
    value: &serde_json::Value,
    label: &str,
    depth: usize,
    budget: &mut ParseBudget,
) -> anyhow::Result<()> {
    if depth > MAX_BUILD_OUTPUT_DEPTH {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: {label} exceeds the {MAX_BUILD_OUTPUT_DEPTH}-level JSON depth bound"
        );
    }
    budget.json_nodes = budget.json_nodes.saturating_add(1);
    if budget.json_nodes > MAX_BUILD_OUTPUT_JSON_NODES {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: config metadata exceeds {MAX_BUILD_OUTPUT_JSON_NODES} JSON nodes"
        );
    }
    match value {
        serde_json::Value::String(text) => validate_json_text(text, label)?,
        serde_json::Value::Array(values) => {
            for value in values {
                validate_json(value, label, depth + 1, budget)?;
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                validate_json_text(key, label)?;
                validate_json(value, label, depth + 1, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_json_text(text: &str, label: &str) -> anyhow::Result<()> {
    if text.len() > MAX_BUILD_OUTPUT_JSON_VALUE_BYTES {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: {label} contains a string over the {MAX_BUILD_OUTPUT_JSON_VALUE_BYTES}-byte bound"
        );
    }
    if text.contains('\0') {
        bail!("BUILD_OUTPUT_V3_INVALID: {label} contains a NUL byte");
    }
    Ok(())
}

fn parse_functions(dir: &Path, budget: &mut ParseBudget) -> anyhow::Result<Vec<DeployedFunction>> {
    let mut functions = Vec::new();
    if !optional_exact_dir(dir, "functions")? {
        return Ok(functions);
    }
    let mut prerenders: BTreeMap<String, (PrerenderConfig, serde_json::Value)> = BTreeMap::new();
    let mut prerender_payloads: BTreeMap<String, ()> = BTreeMap::new();
    collect_function_entries(
        dir,
        dir,
        0,
        &mut functions,
        &mut prerenders,
        &mut prerender_payloads,
        budget,
    )?;
    functions.sort_by(|left, right| left.name.cmp(&right.name));
    for pair in functions.windows(2) {
        if pair[0].name == pair[1].name {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: duplicate function name {:?}",
                pair[0].name
            );
        }
    }
    for function in &mut functions {
        if let Some((typed, raw)) = prerenders.remove(&function.name) {
            if let Some(fallback) = prerender_fallback_path(&function.name, &raw)? {
                if prerender_payloads.remove(&fallback).is_none() {
                    bail!(
                        "BUILD_OUTPUT_V3_INVALID: prerender fallback {fallback:?} for function {:?} is missing",
                        function.name
                    );
                }
                function.prerender_files.push(fallback);
            }
            function.prerender = Some(typed);
            function.raw_prerender = Some(raw);
        }
    }
    if let Some((name, _)) = prerenders.into_iter().next() {
        bail!("BUILD_OUTPUT_V3_INVALID: orphan prerender config for function {name:?}");
    }
    if let Some((path, _)) = prerender_payloads.into_iter().next() {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: unexpected regular file functions/{path}; only referenced prerender fallback payloads are valid outside .func directories"
        );
    }
    Ok(functions)
}

fn collect_function_entries(
    base: &Path,
    dir: &Path,
    depth: usize,
    functions: &mut Vec<DeployedFunction>,
    prerenders: &mut BTreeMap<String, (PrerenderConfig, serde_json::Value)>,
    prerender_payloads: &mut BTreeMap<String, ()>,
    budget: &mut ParseBudget,
) -> anyhow::Result<()> {
    if depth > MAX_BUILD_OUTPUT_DEPTH {
        bail!("BUILD_OUTPUT_V3_INVALID: functions tree exceeds {MAX_BUILD_OUTPUT_DEPTH} levels");
    }
    for entry in sorted_entries(dir)? {
        let path = entry.path();
        let relative = relative_path(base, &path)?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect {relative:?}"))?;
        if file_type.is_symlink() {
            bail!("BUILD_OUTPUT_V3_INVALID: functions/{relative} is a symlink");
        }
        if file_type.is_dir() {
            budget.record_dir(&relative)?;
            if path.extension().and_then(|extension| extension.to_str()) == Some("func") {
                if functions.len() >= MAX_BUILD_OUTPUT_FUNCTIONS {
                    bail!(
                        "BUILD_OUTPUT_V3_INVALID: output contains more than {MAX_BUILD_OUTPUT_FUNCTIONS} functions"
                    );
                }
                functions.push(parse_function(base, path, budget)?);
            } else {
                collect_function_entries(
                    base,
                    &path,
                    depth + 1,
                    functions,
                    prerenders,
                    prerender_payloads,
                    budget,
                )?;
            }
            continue;
        }
        if !file_type.is_file() {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: functions/{relative} is not a regular file or directory"
            );
        }
        let bytes = inspect_regular_file(
            &path,
            &format!("functions/{relative}"),
            MAX_BUILD_OUTPUT_FILE_BYTES,
        )?;
        budget.record_file(&relative)?;
        budget.record_payload(bytes)?;
        let Some(name) = relative.strip_suffix(".prerender-config.json") else {
            if prerender_payloads.insert(relative.clone(), ()).is_some() {
                bail!("BUILD_OUTPUT_V3_INVALID: duplicate prerender payload {relative:?}");
            }
            continue;
        };
        if name.is_empty() {
            bail!("BUILD_OUTPUT_V3_INVALID: prerender config has an empty function name");
        }
        let raw = read_json_regular(
            &path,
            &format!("functions/{relative}"),
            MAX_BUILD_OUTPUT_CONFIG_BYTES,
            budget,
            true,
        )?;
        validate_prerender_config(name, &raw)?;
        let typed: PrerenderConfig = serde_json::from_value(raw.clone())
            .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: malformed functions/{relative}"))?;
        if prerenders.insert(name.to_string(), (typed, raw)).is_some() {
            bail!("BUILD_OUTPUT_V3_INVALID: duplicate prerender config for function {name:?}");
        }
    }
    Ok(())
}

fn parse_function(
    functions_root: &Path,
    dir: PathBuf,
    budget: &mut ParseBudget,
) -> anyhow::Result<DeployedFunction> {
    let name = function_name(functions_root, &dir)?;
    if name.is_empty() {
        bail!("BUILD_OUTPUT_V3_INVALID: .func directory has an empty function name");
    }
    let mut files = Vec::new();
    collect_regular_files(&dir, &dir, 0, &mut files, budget)?;
    files.sort();
    let vc = dir.join(".vc-config.json");
    if !files.iter().any(|file| file == ".vc-config.json") {
        bail!("BUILD_OUTPUT_V3_INVALID: function {name:?} is missing required .vc-config.json");
    }
    let raw_config = read_json_regular(
        &vc,
        &format!("functions/{name}.func/.vc-config.json"),
        MAX_BUILD_OUTPUT_CONFIG_BYTES,
        budget,
        true,
    )?;
    validate_function_config(&name, &raw_config, &files)?;
    let config =
        parse_vc_config(&normalized_typed_function_config(&raw_config)).with_context(|| {
            format!("BUILD_OUTPUT_V3_INVALID: function {name:?} has invalid .vc-config.json")
        })?;
    Ok(DeployedFunction {
        name,
        dir,
        config,
        raw_config,
        files,
        prerender: None,
        raw_prerender: None,
        prerender_files: Vec::new(),
    })
}

fn validate_function_config(
    name: &str,
    config: &serde_json::Value,
    files: &[String],
) -> anyhow::Result<()> {
    let object = config.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "BUILD_OUTPUT_V3_INVALID: function {name:?} .vc-config.json must be an object"
        )
    })?;
    let runtime = object
        .get("runtime")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "BUILD_OUTPUT_V3_INVALID: function {name:?} .vc-config.json requires a non-empty runtime"
            )
        })?;
    validate_json_text(runtime, &format!("function {name:?} runtime"))?;
    let entry_key = if runtime == "edge" {
        "entrypoint"
    } else {
        "handler"
    };
    let entry = object
        .get(entry_key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "BUILD_OUTPUT_V3_INVALID: function {name:?} .vc-config.json requires a non-empty {entry_key}"
            )
        })?;
    let entry = normalized_relative_value(entry, &format!("function {name:?} {entry_key}"))?;
    if runtime == "edge" && files.binary_search(&entry).is_err() {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: edge function {name:?} entrypoint {entry:?} is not an exact regular function file"
        );
    }
    Ok(())
}

/// The legacy typed compatibility view predates the official Edge `regions:
/// "all" | string | string[]` union. Normalize only that view; the durable raw
/// config remains byte-semantically exact in `raw_config`.
fn normalized_typed_function_config(config: &serde_json::Value) -> serde_json::Value {
    let mut typed = config.clone();
    let edge = typed.get("runtime").and_then(serde_json::Value::as_str) == Some("edge");
    if edge {
        if let Some(regions) = typed.get_mut("regions") {
            if let Some(region) = regions.as_str() {
                *regions = if region == "all" {
                    serde_json::Value::Array(Vec::new())
                } else {
                    serde_json::Value::Array(vec![serde_json::Value::String(region.to_string())])
                };
            }
        }
    }
    typed
}

fn validate_prerender_config(name: &str, raw: &serde_json::Value) -> anyhow::Result<()> {
    let object = raw.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "BUILD_OUTPUT_V3_INVALID: prerender config for function {name:?} must be an object"
        )
    })?;
    match object.get("expiration") {
        Some(serde_json::Value::Bool(false)) => {}
        Some(serde_json::Value::Number(number)) if number.as_u64().is_some() => {}
        _ => bail!(
            "BUILD_OUTPUT_V3_INVALID: prerender config for function {name:?} requires expiration as a non-negative integer or false"
        ),
    }
    if object
        .get("group")
        .is_some_and(|value| value.as_u64().is_none())
    {
        bail!("BUILD_OUTPUT_V3_INVALID: prerender group for function {name:?} must be a non-negative integer");
    }
    for field in ["bypassToken", "fallback"] {
        if object
            .get(field)
            .is_some_and(|value| value.as_str().is_none())
        {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: prerender {field} for function {name:?} must be a string"
            );
        }
    }
    if let Some(values) = object.get("allowQuery") {
        let values = values.as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "BUILD_OUTPUT_V3_INVALID: prerender allowQuery for function {name:?} must be an array"
            )
        })?;
        if values.len() > 1024 || values.iter().any(|value| value.as_str().is_none()) {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: prerender allowQuery for function {name:?} must contain at most 1024 strings"
            );
        }
    }
    for field in ["passQuery", "exposeErrBody"] {
        if object
            .get(field)
            .is_some_and(|value| value.as_bool().is_none())
        {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: prerender {field} for function {name:?} must be a boolean"
            );
        }
    }
    if let Some(headers) = object.get("initialHeaders") {
        let headers = headers.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "BUILD_OUTPUT_V3_INVALID: prerender initialHeaders for function {name:?} must be an object"
            )
        })?;
        if headers.len() > 128 || headers.values().any(|value| value.as_str().is_none()) {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: prerender initialHeaders for function {name:?} must contain at most 128 string values"
            );
        }
    }
    if let Some(status) = object.get("initialStatus") {
        let valid = status
            .as_u64()
            .is_some_and(|status| (100..=599).contains(&status));
        if !valid {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: prerender initialStatus for function {name:?} must be in 100..=599"
            );
        }
    }
    Ok(())
}

fn prerender_fallback_path(
    function_name: &str,
    raw: &serde_json::Value,
) -> anyhow::Result<Option<String>> {
    validate_prerender_config(function_name, raw)?;
    let Some(fallback) = raw.get("fallback").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let fallback = normalized_relative_value(fallback, "prerender fallback")?;
    let parent = function_name.rsplit_once('/').map(|(parent, _)| parent);
    let relative = match parent {
        Some(parent) => format!("{parent}/{fallback}"),
        None => fallback,
    };
    normalized_relative_value(&relative, "prerender fallback").map(Some)
}

fn normalized_relative_value(value: &str, label: &str) -> anyhow::Result<String> {
    let mut segments = Vec::new();
    for component in Path::new(value).components() {
        let Component::Normal(segment) = component else {
            bail!("BUILD_OUTPUT_V3_INVALID: {label} {value:?} is not a normalized relative path");
        };
        let segment = segment.to_str().ok_or_else(|| {
            anyhow::anyhow!("BUILD_OUTPUT_V3_INVALID: {label} {value:?} is not UTF-8")
        })?;
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains(['\\', ':', '\0'])
            || segment.chars().any(char::is_control)
        {
            bail!("BUILD_OUTPUT_V3_INVALID: {label} component {segment:?} is not portable");
        }
        segments.push(segment);
    }
    let normalized = segments.join("/");
    if normalized.is_empty() || normalized.len() > MAX_BUILD_OUTPUT_PATH_BYTES {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: {label} length {} is outside 1..={MAX_BUILD_OUTPUT_PATH_BYTES}",
            normalized.len()
        );
    }
    Ok(normalized)
}

fn collect_regular_files(
    base: &Path,
    dir: &Path,
    depth: usize,
    files: &mut Vec<String>,
    budget: &mut ParseBudget,
) -> anyhow::Result<()> {
    if depth > MAX_BUILD_OUTPUT_DEPTH {
        bail!("BUILD_OUTPUT_V3_INVALID: function tree exceeds {MAX_BUILD_OUTPUT_DEPTH} levels");
    }
    for entry in sorted_entries(dir)? {
        let path = entry.path();
        let relative = relative_path(base, &path)?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect {relative:?}"))?;
        if file_type.is_symlink() {
            bail!("BUILD_OUTPUT_V3_INVALID: function file {relative:?} is a symlink");
        }
        if file_type.is_dir() {
            budget.record_dir(&relative)?;
            collect_regular_files(base, &path, depth + 1, files, budget)?;
        } else if file_type.is_file() {
            let bytes = inspect_regular_file(
                &path,
                &format!("function file {relative:?}"),
                MAX_BUILD_OUTPUT_FILE_BYTES,
            )?;
            budget.record_file(&relative)?;
            budget.record_payload(bytes)?;
            files.push(relative);
        } else {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: function entry {relative:?} is not a regular file or directory"
            );
        }
    }
    Ok(())
}

fn function_name(base: &Path, function_dir: &Path) -> anyhow::Result<String> {
    let relative = relative_path(base, function_dir)?;
    relative
        .strip_suffix(".func")
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "BUILD_OUTPUT_V3_INVALID: function directory {relative:?} does not end in .func"
            )
        })
}

/// Flatten `static/` into a sorted list of exact asset paths relative to `static/`.
fn list_static(dir: &Path, budget: &mut ParseBudget) -> anyhow::Result<Vec<String>> {
    let mut files = Vec::new();
    if !optional_exact_dir(dir, "static")? {
        return Ok(files);
    }
    collect_static_files(dir, dir, 0, &mut files, budget)?;
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_static_files(
    base: &Path,
    dir: &Path,
    depth: usize,
    files: &mut Vec<String>,
    budget: &mut ParseBudget,
) -> anyhow::Result<()> {
    if depth > MAX_BUILD_OUTPUT_DEPTH {
        bail!("BUILD_OUTPUT_V3_INVALID: static tree exceeds {MAX_BUILD_OUTPUT_DEPTH} levels");
    }
    for entry in sorted_entries(dir)? {
        let path = entry.path();
        let relative = relative_path(base, &path)?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot inspect {relative:?}"))?;
        if file_type.is_symlink() {
            bail!("BUILD_OUTPUT_V3_INVALID: static/{relative} is a symlink");
        }
        if file_type.is_dir() {
            budget.record_dir(&relative)?;
            collect_static_files(base, &path, depth + 1, files, budget)?;
        } else if file_type.is_file() {
            let bytes = inspect_regular_file(
                &path,
                &format!("static/{relative}"),
                MAX_BUILD_OUTPUT_FILE_BYTES,
            )?;
            budget.record_file(&relative)?;
            budget.record_payload(bytes)?;
            files.push(relative);
        } else {
            bail!("BUILD_OUTPUT_V3_INVALID: static/{relative} is not a regular file or directory");
        }
    }
    Ok(())
}

fn sorted_entries(dir: &Path) -> anyhow::Result<Vec<std::fs::DirEntry>> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("BUILD_OUTPUT_V3_INVALID: cannot read {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "BUILD_OUTPUT_V3_INVALID: cannot enumerate {}",
                dir.display()
            )
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn relative_path(base: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path.strip_prefix(base).map_err(|_| {
        anyhow::anyhow!(
            "BUILD_OUTPUT_V3_INVALID: {} is outside {}",
            path.display(),
            base.display()
        )
    })?;
    let mut segments = Vec::new();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            bail!(
                "BUILD_OUTPUT_V3_INVALID: relative path {:?} is not normalized",
                relative
            );
        };
        let segment = segment.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "BUILD_OUTPUT_V3_INVALID: relative path {:?} is not UTF-8",
                relative
            )
        })?;
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains(['\\', ':', '\0'])
            || segment.chars().any(char::is_control)
        {
            bail!("BUILD_OUTPUT_V3_INVALID: relative path component {segment:?} is not portable");
        }
        segments.push(segment);
    }
    let normalized = segments.join("/");
    if normalized.is_empty() || normalized.len() > MAX_BUILD_OUTPUT_PATH_BYTES {
        bail!(
            "BUILD_OUTPUT_V3_INVALID: normalized path length {} is outside 1..={MAX_BUILD_OUTPUT_PATH_BYTES}",
            normalized.len()
        );
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn parses_a_build_output() {
        let tmp = std::env::temp_dir().join(format!("fb-test-{}", std::process::id()));
        let out = tmp.join(".vercel/output");
        let _ = fs::remove_dir_all(&tmp);

        write(
            &out.join("config.json"),
            r#"{
            "version": 3,
            "routes": [
                { "handle": "filesystem" },
                { "src": "/api/(.*)", "dest": "/api/$1" }
            ],
            "images": { "sizes": [640, 1080], "formats": ["image/avif"] },
            "crons": [{ "path": "/api/cron", "schedule": "0 0 * * *" }]
        }"#,
        );

        write(
            &out.join("functions/api/hello.func/.vc-config.json"),
            r#"{
            "runtime": "nodejs20.x", "handler": "index.js", "memory": 1024, "maxDuration": 10
        }"#,
        );
        write(
            &out.join("functions/api/hello.func/index.js"),
            "export default () => {}",
        );

        write(
            &out.join("functions/middleware.func/.vc-config.json"),
            r#"{
            "runtime": "edge", "entrypoint": "middleware.js"
        }"#,
        );
        write(
            &out.join("functions/middleware.func/middleware.js"),
            "export default () => {}",
        );

        write(&out.join("static/index.html"), "<!doctype html>");
        write(&out.join("static/assets/app.css"), "body{}");

        let bo = parse_build_output(&tmp).unwrap();
        assert_eq!(bo.config.version, 3);
        assert_eq!(bo.config.routes.len(), 2);
        assert!(bo.has_image_optimization());
        assert_eq!(bo.serverless_count(), 1);
        assert_eq!(bo.edge_count(), 1);
        assert!(bo.functions.iter().any(|f| f.name == "api/hello"));
        assert!(bo.static_files.contains(&"index.html".to_string()));
        assert!(bo.static_files.contains(&"assets/app.css".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }
}
