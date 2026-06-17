//! Read a `.vercel/output` directory (Build Output API v3) into a typed
//! [`BuildOutput`] the platform can provision from.

use std::path::{Path, PathBuf};

use crate::build_output::{parse_vc_config, BuildOutputConfig, FunctionConfig, PrerenderConfig};

/// One function discovered under `functions/<name>.func/`.
#[derive(Clone, Debug)]
pub struct DeployedFunction {
    /// Route-relative name, e.g. "api/hello" (from `api/hello.func`).
    pub name: String,
    /// Absolute path to the `.func` directory.
    pub dir: PathBuf,
    pub config: FunctionConfig,
    /// ISR/prerender metadata if a sibling `<name>.prerender-config.json` exists.
    pub prerender: Option<PrerenderConfig>,
}

/// A fully parsed Build Output.
#[derive(Clone, Debug)]
pub struct BuildOutput {
    pub config: BuildOutputConfig,
    pub functions: Vec<DeployedFunction>,
    /// Static asset paths, relative to `static/`.
    pub static_files: Vec<String>,
    /// Absolute path to `.vercel/output`.
    pub root: PathBuf,
}

impl BuildOutput {
    pub fn serverless_count(&self) -> usize {
        self.functions.iter().filter(|f| !f.config.is_edge()).count()
    }
    pub fn edge_count(&self) -> usize {
        self.functions.iter().filter(|f| f.config.is_edge()).count()
    }
    pub fn has_image_optimization(&self) -> bool {
        self.config.images.is_some()
    }
}

/// True if `repo` already contains a Build Output (i.e. `vercel build` ran, or a
/// framework adapter produced one).
pub fn has_build_output(repo: &Path) -> bool {
    repo.join(".vercel/output/config.json").is_file()
}

/// Parse `<repo>/.vercel/output` into a [`BuildOutput`].
pub fn parse_build_output(repo: &Path) -> anyhow::Result<BuildOutput> {
    let root = repo.join(".vercel/output");
    let config_path = root.join("config.json");
    let config: BuildOutputConfig = serde_json::from_str(&std::fs::read_to_string(&config_path)?)?;

    let functions = parse_functions(&root.join("functions"))?;
    let static_files = list_static(&root.join("static"));

    Ok(BuildOutput { config, functions, static_files, root })
}

fn parse_functions(dir: &Path) -> anyhow::Result<Vec<DeployedFunction>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    collect_func_dirs(dir, dir, &mut out)?;
    Ok(out)
}

/// Recursively find `*.func` directories (functions can be nested, e.g.
/// `api/users/[id].func`).
fn collect_func_dirs(base: &Path, dir: &Path, out: &mut Vec<DeployedFunction>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("func") {
            let vc = path.join(".vc-config.json");
            if vc.is_file() {
                let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&vc)?)?;
                let config = parse_vc_config(&v)?;
                let name = func_name(base, &path);
                let prerender = read_prerender(base, &name);
                out.push(DeployedFunction { name, dir: path, config, prerender });
            }
        } else {
            // Descend into ordinary directories that may contain nested funcs.
            collect_func_dirs(base, &path, out)?;
        }
    }
    Ok(())
}

/// `functions/api/hello.func` (base = functions) -> "api/hello".
fn func_name(base: &Path, func_dir: &Path) -> String {
    let rel = func_dir.strip_prefix(base).unwrap_or(func_dir);
    let s = rel.to_string_lossy();
    s.strip_suffix(".func").unwrap_or(&s).to_string()
}

fn read_prerender(base: &Path, name: &str) -> Option<PrerenderConfig> {
    let p = base.join(format!("{name}.prerender-config.json"));
    let txt = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&txt).ok()
}

/// Flatten `static/` into a list of asset paths relative to `static/`.
fn list_static(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
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

        write(&out.join("config.json"), r#"{
            "version": 3,
            "routes": [
                { "handle": "filesystem" },
                { "src": "/api/(.*)", "dest": "/api/$1" }
            ],
            "images": { "sizes": [640, 1080], "formats": ["image/avif"] },
            "crons": [{ "path": "/api/cron", "schedule": "0 0 * * *" }]
        }"#);

        // a serverless function
        write(&out.join("functions/api/hello.func/.vc-config.json"), r#"{
            "runtime": "nodejs20.x", "handler": "index.js", "memory": 1024, "maxDuration": 10
        }"#);
        write(&out.join("functions/api/hello.func/index.js"), "export default () => {}");

        // an edge middleware function
        write(&out.join("functions/middleware.func/.vc-config.json"), r#"{
            "runtime": "edge", "entrypoint": "middleware.js"
        }"#);
        write(&out.join("functions/middleware.func/middleware.js"), "export default () => {}");

        // static assets
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
