//! Operator diagnostic (same family as --node-probe / --swarm-probe): prove
//! Vue.js is supported end to end, by live execution of the real build and
//! deployment primitives. Starts no node, binds no port, writes only to a
//! scratch directory that is removed on every exit path.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        let root = if cfg!(target_os = "macos") {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        }
        .join(format!("hive-vue-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).context("create Vue probe scratch dir")?;
        Ok(Self(root))
    }

    fn repo(&self, name: &str, files: &[(&str, &str)]) -> Result<PathBuf> {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).context("create Vue probe fixture dir")?;
        for (file, body) in files {
            let path = dir.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).context("create Vue probe fixture parent")?;
            }
            std::fs::write(&path, body).context("write Vue probe fixture")?;
        }
        Ok(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const VUE_CLI: &str = r#"{"name":"vue-cli-app","scripts":{"build":"vue-cli-service build"},"dependencies":{"vue":"^3.3.4"},"devDependencies":{"@vue/cli-service":"~5.0.8"}}"#;
const VUE_VITE: &str = r#"{"name":"vue-vite-app","scripts":{"build":"vite build"},"dependencies":{"vue":"^3.5.46"},"devDependencies":{"@vitejs/plugin-vue":"^6.0.11","vite":"^8.3.8"}}"#;
const PLAIN_VITE: &str = r#"{"name":"plain-vite-app","scripts":{"build":"vite build"},"devDependencies":{"vite":"^8.3.8"}}"#;
const NUXT: &str = r#"{"name":"nuxt-app","dependencies":{"vue":"^3.5.46"},"devDependencies":{"nuxt":"^3.0.0","vite":"^8.3.8"}}"#;
const WORKSPACE_ROOT: &str = r#"{"private":true,"workspaces":["apps/*"]}"#;

/// One Vue.js framework preset, but two build lanes. A project script is the
/// authoritative build command for both, and pmify rewrites it per manager.
fn assert_plan(dir: &Path, label: &str, want_slug: &str, want_build: &str) -> Result<()> {
    let a = fluid_build::analyze(dir);
    if a.framework_slug != want_slug {
        bail!("{label}: framework_slug {} != {want_slug}", a.framework_slug);
    }
    if a.build_command != want_build {
        bail!("{label}: build_command {:?} != {want_build:?}", a.build_command);
    }
    if a.output_dir != "dist" {
        bail!("{label}: output_dir {:?} != dist", a.output_dir);
    }
    if a.primitive != fluid_build::Primitive::Static {
        bail!("{label}: primitive {:?} is not Static", a.primitive);
    }
    println!(
        "RESULT plan={label} PASS slug={} build={:?} output=dist",
        a.framework_slug, a.build_command
    );
    Ok(())
}

pub async fn run() -> Result<()> {
    let s = Scratch::new()?;

    let cli = s.repo(
        "vue-cli",
        &[
            ("package.json", VUE_CLI),
            ("vue.config.js", "module.exports = {}"),
        ],
    )?;
    assert_plan(&cli, "vue_cli", "vue", "npm run build")?;

    let vite = s.repo(
        "vue-vite",
        &[
            ("package.json", VUE_VITE),
            ("vite.config.ts", "export default {}"),
            ("index.html", "<!doctype html>"),
        ],
    )?;
    assert_plan(&vite, "vue_vite", "vue", "npm run build")?;

    let pnpm = s.repo(
        "vue-vite-pnpm",
        &[
            ("package.json", VUE_VITE),
            ("vite.config.mts", "export default {}"),
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
        ],
    )?;
    assert_plan(&pnpm, "vue_vite_pnpm", "vue", "pnpm run build")?;

    let plain = s.repo(
        "plain-vite",
        &[
            ("package.json", PLAIN_VITE),
            ("vite.config.js", "export default {}"),
        ],
    )?;
    let plain_a = fluid_build::analyze(&plain);
    if plain_a.framework_slug != "vite" {
        bail!("plain Vite was misdetected as {}", plain_a.framework_slug);
    }
    println!("RESULT registry=PASS plain_vite_stays={}", plain_a.framework_slug);

    let nuxt = s.repo(
        "nuxt",
        &[
            ("package.json", NUXT),
            ("nuxt.config.ts", "export default {}"),
            ("vite.config.ts", "export default {}"),
        ],
    )?;
    let nuxt_a = fluid_build::analyze(&nuxt);
    if nuxt_a.framework_slug != "nuxtjs" {
        bail!("Nuxt was misdetected as {}", nuxt_a.framework_slug);
    }
    println!("RESULT registry=PASS nuxt_stays={}", nuxt_a.framework_slug);

    for name in ["Vue.js", "vuejs", "Vue 3", "vue"] {
        let Some(preset) = fluid_build::preset_by_name(name) else {
            bail!("preset_by_name({name:?}) refused an explicit Vue.js selection");
        };
        if preset.slug != "vue" {
            bail!("preset_by_name({name:?}) resolved to {}", preset.slug);
        }
    }
    let catalog = fluid_build::PRESETS;
    let vue = catalog
        .iter()
        .find(|p| p.slug == "vue")
        .ok_or_else(|| anyhow::anyhow!("framework catalog has no vue preset"))?;
    if vue.build_command != "npm run build" {
        bail!("catalog vue build_command is {}", vue.build_command);
    }
    println!(
        "RESULT catalog=PASS presets={} vue_name={} vue_build={}",
        catalog.len(),
        vue.name,
        vue.build_command
    );

    // Static deployments get a catch-all to /index.html, so Vue Router history
    // mode resolves on a deep link instead of 404ing.
    let cfg = fluid_build::synthesize_config(fluid_build::Primitive::Static);
    let spa = cfg
        .routes
        .iter()
        .any(|r| r.dest.as_deref() == Some("/index.html"));
    if !spa {
        bail!("Static deployment has no SPA fallback");
    }
    println!("RESULT deploy=PASS spa_fallback={spa}");

    // Deployment classification: a Vue 3 + Vite workspace declares BOTH vue
    // and vite, which previously produced conflicting framework evidence.
    let root = s.repo(
        "workspace",
        &[
            ("package.json", WORKSPACE_ROOT),
            ("pnpm-workspace.yaml", "packages:\n  - apps/*\n"),
            ("apps/web/package.json", VUE_VITE),
            ("apps/web/vite.config.ts", "export default {}"),
            ("apps/web/index.html", "<!doctype html>"),
        ],
    )?;
    let ws = crate::workspace::Workspace {
        source: "pnpm-workspace.yaml",
        members: vec![PathBuf::from("apps/web")],
    };
    let selection = crate::app_discovery::select(&root, Some(&ws))
        .await?
        .ok_or_else(|| anyhow::anyhow!("Vue workspace selected no application"))?;
    let has_vue = selection
        .evidence
        .iter()
        .any(|e| e == "framework:vue");
    let has_vite = selection
        .evidence
        .iter()
        .any(|e| e == "framework:vite");
    if !has_vue || has_vite {
        bail!(
            "Vue workspace evidence {:?} lacks framework:vue or still names vite",
            selection.evidence
        );
    }
    println!(
        "RESULT discovery=PASS selected={} evidence={:?}",
        selection.relative.display(),
        selection.evidence
    );

    println!("VUE_PROBE=PASS");
    Ok(())
}
