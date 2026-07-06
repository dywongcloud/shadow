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
pub mod framework;
pub mod nextjs;
pub mod parser;
pub mod per_route;
pub mod vercel_config;

pub use build_output::{BuildOutputConfig, FunctionConfig, Route, BUILD_OUTPUT_VERSION};
pub use framework::{
    detect, detect_package_manager, package_manager, plan_build, BuildPlan, FrameworkPreset, PackageManagerDetection,
    PackageManagerSource, Primitive, PRESETS,
};
pub use nextjs::{detect_features, BuildFeatures};
pub use parser::{has_build_output, parse_build_output, BuildOutput, DeployedFunction};
pub use vercel_config::{
    load_vercel_config, ConditionValue, VercelCondition, VercelConfig, VercelCron, VercelFunction,
    VercelHeader, VercelHeaderRule, VercelImages, VercelRedirect, VercelRewrite,
};

use serde::Serialize;
use std::path::Path;

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
        install_command: plan.install_command,
        build_command: plan.build_command,
        output_dir: plan.output_dir,
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

pub fn resolve(repo: &Path) -> anyhow::Result<Resolution> {
    if has_build_output(repo) {
        Ok(Resolution::Ready(parse_build_output(repo)?))
    } else {
        Ok(Resolution::NeedsBuild(plan_build(repo, None, None, None, None)))
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
                Route { handle: Some("filesystem".into()), ..Default::default() },
                Route { src: Some("/(.*)".into()), dest: Some("/index.html".into()), ..Default::default() },
            ];
        }
        Primitive::Serverless | Primitive::Hybrid => {
            // Static assets first, then everything else hits the function.
            cfg.routes = vec![
                Route { handle: Some("filesystem".into()), ..Default::default() },
                Route { src: Some("/(.*)".into()), dest: Some("/index".into()), ..Default::default() },
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
        fs::write(dir.join("package.json"), r#"{"dependencies":{"next":"14"}}"#).unwrap();

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
        assert!(cfg.routes.iter().any(|r| r.dest.as_deref() == Some("/index.html")));
    }
}
