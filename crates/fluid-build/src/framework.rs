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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageManagerSource {
    /// `package.json#packageManager` (Corepack) — the strongest signal; wins
    /// over every lockfile per npm/Corepack's own documented precedence.
    Corepack,
    BunLock,
    PnpmLock,
    YarnLock,
    NpmLock,
    /// No signal at all — the platform default.
    Default,
}

/// Full package-manager detection result, with provenance and (if more than
/// one manager's lockfile is present) a deterministic conflict warning.
#[derive(Clone, Debug, Serialize)]
pub struct PackageManagerDetection {
    pub manager: &'static str,
    pub source: PackageManagerSource,
    /// Set when a lockfile for a DIFFERENT manager than the winner is also
    /// present — e.g. both `bun.lock` and `pnpm-lock.yaml` committed. Never
    /// silently dropped: this string is meant to be logged verbatim.
    pub conflict_warning: Option<String>,
}

/// Parse `package.json#packageManager` (Corepack), e.g. `"pnpm@8.15.4"` ->
/// `"pnpm"`. `None` if the field is absent, unparseable, or names a manager
/// this platform doesn't recognize (falls through to lockfile detection).
fn corepack_package_manager(repo: &Path) -> Option<&'static str> {
    let pkg = std::fs::read_to_string(repo.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&pkg).ok()?;
    let raw = v.get("packageManager")?.as_str()?;
    match raw.split('@').next().unwrap_or(raw).trim() {
        "bun" => Some("bun"),
        "pnpm" => Some("pnpm"),
        "yarn" => Some("yarn"),
        "npm" => Some("npm"),
        _ => None,
    }
}

/// Detect the JS package manager with full provenance. Precedence (exact):
/// `package.json#packageManager` (Corepack) > `bun.lock` > `bun.lockb` >
/// `pnpm-lock.yaml` > `yarn.lock` > `package-lock.json` > default (npm).
/// Corepack wins over every lockfile — it's an explicit user choice, whereas a
/// lockfile is just evidence of which manager last ran `install`. Never
/// deletes or mutates any lockfile; a conflicting one is only ever reported,
/// never removed.
pub fn detect_package_manager(repo: &Path) -> PackageManagerDetection {
    let corepack = corepack_package_manager(repo);
    let bun_lock = repo.join("bun.lock").exists() || repo.join("bun.lockb").exists();
    let pnpm_lock = repo.join("pnpm-lock.yaml").exists();
    let yarn_lock = repo.join("yarn.lock").exists();
    let npm_lock = repo.join("package-lock.json").exists();

    let (manager, source) = corepack
        .map(|pm| (pm, PackageManagerSource::Corepack))
        .or_else(|| bun_lock.then_some(("bun", PackageManagerSource::BunLock)))
        .or_else(|| pnpm_lock.then_some(("pnpm", PackageManagerSource::PnpmLock)))
        .or_else(|| yarn_lock.then_some(("yarn", PackageManagerSource::YarnLock)))
        .or_else(|| npm_lock.then_some(("npm", PackageManagerSource::NpmLock)))
        .unwrap_or(("npm", PackageManagerSource::Default));

    // Any OTHER manager's lockfile present alongside the winner is a
    // conflicting signal — always surfaced, regardless of whether the winner
    // came from Corepack or lockfile precedence.
    let mut conflicting: Vec<&str> = Vec::new();
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
    let conflict_warning = (!conflicting.is_empty()).then(|| {
        format!(
            "Multiple package-manager signals detected — using \"{manager}\" ({source:?}); ignoring conflicting lockfile(s): {}.",
            conflicting.join(", ")
        )
    });

    PackageManagerDetection {
        manager,
        source,
        conflict_warning,
    }
}

/// Detect the JS package manager from `package.json#packageManager` (Corepack)
/// and lockfiles (Vercel's precedence: bun -> pnpm -> yarn -> npm). Defaults to
/// npm. Thin wrapper over [`detect_package_manager`] for callers that only need
/// the manager name, not the full provenance/conflict diagnostics.
pub fn package_manager(repo: &Path) -> &'static str {
    detect_package_manager(repo).manager
}

/// The install command for a package manager.
fn install_for(pm: &str) -> &'static str {
    match pm {
        "bun" => "bun install",
        "pnpm" => "pnpm install --no-frozen-lockfile",
        "yarn" => "yarn install",
        _ => "npm install",
    }
}

/// Rewrite an `npm …` command to the detected package manager so script/binary
/// invocations use the project's actual tool (`pnpm run build`, `yarn build`…).
fn pmify(cmd: &str, pm: &str) -> String {
    if pm == "npm" {
        return cmd.to_string();
    }
    let c = cmd.trim();
    if let Some(rest) = c.strip_prefix("npm run ") {
        return match pm {
            "yarn" => format!("yarn {rest}"),
            _ => format!("{pm} run {rest}"),
        };
    }
    if c == "npm install" || c == "npm i" {
        return install_for(pm).to_string();
    }
    if let Some(rest) = c.strip_prefix("npm exec ") {
        return format!("{pm} exec {rest}");
    }
    cmd.to_string()
}

/// Concrete plan for building one repo.
#[derive(Clone, Debug, Serialize)]
pub struct BuildPlan {
    pub framework: FrameworkPreset,
    /// Detected package manager: npm | yarn | pnpm | bun.
    pub package_manager: String,
    pub install_command: String,
    pub build_command: String,
    pub output_dir: String,
}

/// Detect the framework for a repo by inspecting marker files + package.json
/// dependencies. Order matters: most specific first.
pub fn detect(repo: &Path) -> &'static FrameworkPreset {
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
    if let Ok(pkg) = std::fs::read_to_string(repo.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
            if has_dep(&v, "@opennextjs/aws") || has_dep(&v, "open-next") {
                return preset("opennext").unwrap();
            }
            if has_dep(&v, "vinext") {
                return preset("vinext").unwrap();
            }
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
            if let Some(p) = preset(slug) {
                return p;
            }
        }
    }

    // 2) package.json dependency sniffing.
    if let Ok(pkg) = std::fs::read_to_string(repo.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
            let dep = |name: &str| has_dep(&v, name);
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
/// Find a preset by slug or display name (case-insensitive), so an explicit
/// framework choice in project settings can override auto-detection.
pub fn preset_by_name(name: &str) -> Option<&'static FrameworkPreset> {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() {
        return None;
    }
    PRESETS
        .iter()
        .find(|p| p.slug.eq_ignore_ascii_case(&n) || p.name.to_ascii_lowercase() == n)
}

pub fn plan_build(
    repo: &Path,
    framework_override: Option<&str>,
    install_override: Option<&str>,
    build_override: Option<&str>,
    output_override: Option<&str>,
) -> BuildPlan {
    // An explicit framework choice (project settings) wins over auto-detection.
    let fw = framework_override
        .and_then(preset_by_name)
        .cloned()
        .unwrap_or_else(|| detect(repo).clone());
    let pm = package_manager(repo);
    let pick = |ov: Option<&str>, default: &str| {
        ov.map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_string()
    };
    // Default install command follows the package manager; framework binary build
    // commands (e.g. "next build") resolve via node_modules/.bin, while "npm run …"
    // defaults are rewritten to the detected manager.
    let install_default = install_for(pm);
    BuildPlan {
        install_command: pick(install_override, install_default),
        build_command: pmify(&pick(build_override, fw.build_command), pm),
        output_dir: pick(output_override, fw.output_dir),
        package_manager: pm.to_string(),
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

        let dir = repo_with(&[("yarn.lock", "")]);
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
        assert_eq!(d.source, PackageManagerSource::Corepack);
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
        assert_eq!(d.source, PackageManagerSource::Corepack);
        assert!(d.conflict_warning.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn conflicting_lockfiles_without_corepack_field_still_warn_deterministically() {
        // No packageManager field — lockfile precedence picks bun, but a
        // pnpm-lock.yaml is ALSO committed (e.g. a half-migrated repo). Must
        // still resolve deterministically (bun wins, per precedence) AND warn.
        let dir = repo_with(&[("bun.lock", ""), ("pnpm-lock.yaml", "")]);
        let d = detect_package_manager(&dir);
        assert_eq!(d.manager, "bun");
        assert_eq!(d.source, PackageManagerSource::BunLock);
        let warning = d
            .conflict_warning
            .expect("must warn about the conflicting pnpm-lock.yaml");
        assert!(warning.contains("pnpm-lock.yaml"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_or_unrecognized_package_manager_field_falls_back_to_lockfile() {
        // Unparseable JSON → falls through to lockfile detection, never panics.
        let dir = repo_with(&[("package.json", "{not json"), ("yarn.lock", "")]);
        assert_eq!(package_manager(&dir), "yarn");
        let _ = fs::remove_dir_all(&dir);

        // Recognized JSON but an unknown manager name → also falls through.
        let dir = repo_with(&[
            ("package.json", r#"{"packageManager":"deno@1.0.0"}"#),
            ("yarn.lock", ""),
        ]);
        assert_eq!(package_manager(&dir), "yarn");
        let _ = fs::remove_dir_all(&dir);
    }
}
