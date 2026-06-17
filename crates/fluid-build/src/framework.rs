//! Framework detection + build planning — the front half of Framework-Defined
//! Infrastructure. Given a repo we identify the framework (à la Vercel's 35+
//! presets), then produce a [`BuildPlan`]: the install/build commands, the
//! native output directory, and which **primitive** the output maps to.

use serde::Serialize;
use std::path::Path;

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
    FrameworkPreset { slug: "nextjs", name: "Next.js", install_command: "npm install", build_command: "next build", dev_command: "next dev", output_dir: ".next", primitive: Primitive::Hybrid, emits_build_output: true },
    FrameworkPreset { slug: "nuxtjs", name: "Nuxt", install_command: "npm install", build_command: "nuxt build", dev_command: "nuxt dev", output_dir: ".output", primitive: Primitive::Hybrid, emits_build_output: true },
    FrameworkPreset { slug: "sveltekit", name: "SvelteKit", install_command: "npm install", build_command: "vite build", dev_command: "vite dev", output_dir: ".svelte-kit", primitive: Primitive::Hybrid, emits_build_output: true },
    FrameworkPreset { slug: "remix", name: "Remix", install_command: "npm install", build_command: "remix vite:build", dev_command: "remix vite:dev", output_dir: "build", primitive: Primitive::Hybrid, emits_build_output: true },
    FrameworkPreset { slug: "astro", name: "Astro", install_command: "npm install", build_command: "astro build", dev_command: "astro dev", output_dir: "dist", primitive: Primitive::Hybrid, emits_build_output: false },
    FrameworkPreset { slug: "gatsby", name: "Gatsby", install_command: "npm install", build_command: "gatsby build", dev_command: "gatsby develop", output_dir: "public", primitive: Primitive::Static, emits_build_output: false },
    FrameworkPreset { slug: "vite", name: "Vite", install_command: "npm install", build_command: "vite build", dev_command: "vite", output_dir: "dist", primitive: Primitive::Static, emits_build_output: false },
    FrameworkPreset { slug: "create-react-app", name: "Create React App", install_command: "npm install", build_command: "react-scripts build", dev_command: "react-scripts start", output_dir: "build", primitive: Primitive::Static, emits_build_output: false },
    FrameworkPreset { slug: "vue", name: "Vue", install_command: "npm install", build_command: "vue-cli-service build", dev_command: "vue-cli-service serve", output_dir: "dist", primitive: Primitive::Static, emits_build_output: false },
    FrameworkPreset { slug: "node", name: "Node.js Server", install_command: "npm install", build_command: "npm run build", dev_command: "npm start", output_dir: ".", primitive: Primitive::Serverless, emits_build_output: false },
    FrameworkPreset { slug: "static", name: "Static", install_command: "", build_command: "", dev_command: "", output_dir: ".", primitive: Primitive::Static, emits_build_output: false },
];

pub fn preset(slug: &str) -> Option<&'static FrameworkPreset> {
    PRESETS.iter().find(|p| p.slug == slug)
}

/// Concrete plan for building one repo.
#[derive(Clone, Debug, Serialize)]
pub struct BuildPlan {
    pub framework: FrameworkPreset,
    pub install_command: String,
    pub build_command: String,
    pub output_dir: String,
}

/// Detect the framework for a repo by inspecting marker files + package.json
/// dependencies. Order matters: most specific first.
pub fn detect(repo: &Path) -> &'static FrameworkPreset {
    // 1) Config-file markers (cheapest, most reliable).
    let markers: &[(&str, &str)] = &[
        ("next.config.js", "nextjs"), ("next.config.mjs", "nextjs"), ("next.config.ts", "nextjs"),
        ("nuxt.config.js", "nuxtjs"), ("nuxt.config.ts", "nuxtjs"),
        ("svelte.config.js", "sveltekit"),
        ("astro.config.mjs", "astro"), ("astro.config.js", "astro"), ("astro.config.ts", "astro"),
        ("gatsby-config.js", "gatsby"), ("gatsby-config.ts", "gatsby"),
        ("remix.config.js", "remix"),
        ("vite.config.js", "vite"), ("vite.config.ts", "vite"),
        ("vue.config.js", "vue"),
    ];
    for (file, slug) in markers {
        if repo.join(file).exists() {
            if let Some(p) = preset(slug) {
                return p;
            }
        }
    }

    // 2) package.json dependency sniffing.
    if let Ok(pkg) = std::fs::read_to_string(repo.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
            let dep = |name: &str| has_dep(&v, name);
            if dep("next") { return preset("nextjs").unwrap(); }
            if dep("nuxt") || dep("nuxt3") { return preset("nuxtjs").unwrap(); }
            if dep("@sveltejs/kit") { return preset("sveltekit").unwrap(); }
            if dep("@remix-run/dev") { return preset("remix").unwrap(); }
            if dep("astro") { return preset("astro").unwrap(); }
            if dep("gatsby") { return preset("gatsby").unwrap(); }
            if dep("react-scripts") { return preset("create-react-app").unwrap(); }
            if dep("vite") { return preset("vite").unwrap(); }
            if dep("@vue/cli-service") { return preset("vue").unwrap(); }
            // A server start script => treat as a Node serverless app.
            if v.get("scripts").and_then(|s| s.get("start")).is_some() {
                return preset("node").unwrap();
            }
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
pub fn plan_build(
    repo: &Path,
    install_override: Option<&str>,
    build_override: Option<&str>,
    output_override: Option<&str>,
) -> BuildPlan {
    let fw = detect(repo).clone();
    let pick = |ov: Option<&str>, default: &str| {
        ov.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(default).to_string()
    };
    BuildPlan {
        install_command: pick(install_override, fw.install_command),
        build_command: pick(build_override, fw.build_command),
        output_dir: pick(output_override, fw.output_dir),
        framework: fw,
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
        let dir = repo_with(&[("package.json", r#"{"dependencies":{"next":"14.0.0","react":"18"}}"#)]);
        let p = detect(&dir);
        assert_eq!(p.slug, "nextjs");
        assert_eq!(p.primitive, Primitive::Hybrid);
        assert!(p.emits_build_output);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_vite_static() {
        let dir = repo_with(&[("vite.config.ts", "export default {}"), ("package.json", "{}")]);
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
        let plan = plan_build(&dir, None, Some("pnpm build"), None);
        assert_eq!(plan.build_command, "pnpm build");
        assert_eq!(plan.framework.slug, "nextjs");
        let _ = fs::remove_dir_all(&dir);
    }
}
