//! `cargo egui-ios` — scaffold, cross-compile, and run native egui iOS apps via xtool.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

mod plugin_cmd;

// Embedded so a `cargo install`ed binary is fully self-contained.
const TPL_CARGO: &str = include_str!("../../../template/Cargo.toml");
const TPL_LIB: &str = include_str!("../../../template/lib.rs");
const TPL_PACKAGE: &str = include_str!("../../../template/Package.swift");
const TPL_APP: &str = include_str!("../../../template/App.swift");
const TPL_XTOOL: &str = include_str!("../../../template/xtool.yml");
const TPL_INFO: &str = include_str!("../../../template/Info.override.plist");
const TPL_GITIGNORE: &str = include_str!("../../../template/gitignore");
const SHIM_XCRUN: &str = include_str!("../shims/xcrun");
const SHIM_CODESIGN: &str = include_str!("../shims/codesign");

const DEVICE_TRIPLE: &str = "aarch64-apple-ios";
const SIM_TRIPLE: &str = "aarch64-apple-ios-sim";

#[derive(Parser)]
#[command(bin_name = "cargo")]
enum CargoCli {
    #[command(name = "egui-ios", version, about)]
    EguiIos(Cli),
}

#[derive(Args)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scaffold a new egui iOS app in a new directory.
    New {
        /// App / directory name (e.g. my-cool-app).
        name: String,
        /// Bundle identifier (default: com.example.<name>).
        #[arg(long)]
        bundle_id: Option<String>,
        /// Display name shown on the home screen (default: <name>).
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Cross-compile the Rust staticlib for iOS.
    Build(BuildArgs),
    /// Cross-compile, then run on a connected device via `xtool dev`.
    Run(BuildArgs),
    /// Manage WASM UI plugins (scaffold, build, serve for hot reload).
    Plugin {
        #[command(subcommand)]
        cmd: plugin_cmd::PluginCmd,
    },
}

#[derive(Args, Clone)]
struct BuildArgs {
    /// Build for the simulator target (only meaningful on a macOS host).
    #[arg(long)]
    simulator: bool,
    /// Bundle an assets directory into the app (rsync, dereferencing symlinks).
    #[arg(long)]
    assets: Option<PathBuf>,
}

fn main() -> Result<()> {
    let CargoCli::EguiIos(cli) = CargoCli::parse();
    match cli.cmd {
        Cmd::New {
            name,
            bundle_id,
            display_name,
        } => cmd_new(&name, bundle_id, display_name),
        Cmd::Build(args) => {
            cmd_build(&args)?;
            Ok(())
        }
        Cmd::Run(args) => cmd_run(&args),
        Cmd::Plugin { cmd } => plugin_cmd::run(cmd),
    }
}

fn project_ident(name: &str) -> String {
    name.replace(['-', ' '], "_")
}

/// Apple App ID names allow only letters, numbers, and spaces, so the bundle-id segment derived
/// from the project name must be alphanumeric (no underscores or hyphens).
fn bundle_segment(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn render(tpl: &str, project_name: &str, display_name: &str, bundle_id: &str) -> String {
    tpl.replace("{{project_name}}", project_name)
        .replace("{{display_name}}", display_name)
        .replace("{{bundle_id}}", bundle_id)
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn cmd_new(name: &str, bundle_id: Option<String>, display_name: Option<String>) -> Result<()> {
    let root = PathBuf::from(name);
    if root.exists() {
        bail!("{} already exists", root.display());
    }
    let project_name = project_ident(name);
    let display_name = display_name.unwrap_or_else(|| name.to_string());
    let bundle_id = bundle_id.unwrap_or_else(|| format!("com.example.{}", bundle_segment(name)));

    let r = |tpl: &str| render(tpl, &project_name, &display_name, &bundle_id);

    write(&root.join("rust/Cargo.toml"), &r(TPL_CARGO))?;
    write(&root.join("rust/src/lib.rs"), &r(TPL_LIB))?;
    write(&root.join("Package.swift"), &r(TPL_PACKAGE))?;
    write(
        &root.join(format!("Sources/{project_name}/App.swift")),
        &r(TPL_APP),
    )?;
    write(&root.join("xtool.yml"), &r(TPL_XTOOL))?;
    write(&root.join("Info.override.plist"), &r(TPL_INFO))?;
    write(&root.join(".gitignore"), TPL_GITIGNORE)?;

    println!("Created {} ({project_name})", root.display());
    println!("Next:");
    println!("  cd {name}");
    println!("  # edit rust/src/lib.rs");
    println!("  cargo egui-ios run");
    Ok(())
}

/// Query `cargo metadata` for the target directory (correct for workspaces and CARGO_TARGET_DIR).
pub(crate) fn resolve_target_dir(manifest: &Path) -> Result<PathBuf> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--manifest-path"])
        .arg(manifest)
        .output()
        .context("running cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata failed");
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let td = meta["target_directory"]
        .as_str()
        .context("cargo metadata missing target_directory")?;
    Ok(PathBuf::from(td))
}

/// Materialize the xcrun/codesign shims into a temp dir and return it.
fn prepare_shims() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("egui-ios-shims");
    fs::create_dir_all(&dir)?;
    for (name, body) in [("xcrun", SHIM_XCRUN), ("codesign", SHIM_CODESIGN)] {
        let p = dir.join(name);
        fs::write(&p, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(dir)
}

/// Cross-compile and return the absolute Rust target dir (for `-L` linking).
fn cmd_build(args: &BuildArgs) -> Result<PathBuf> {
    let root = std::env::current_dir()?;
    let manifest = root.join("rust/Cargo.toml");
    if !manifest.exists() {
        bail!(
            "no rust/Cargo.toml under {} — run from an egui-ios app directory",
            root.display()
        );
    }

    let triple = if args.simulator { SIM_TRIPLE } else { DEVICE_TRIPLE };
    if args.simulator && cfg!(target_os = "linux") {
        eprintln!("warning: the iOS simulator is unavailable on Linux; this will not run.");
    }

    let shims = prepare_shims()?;
    let path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut paths = vec![shims.clone()];
            paths.extend(std::env::split_paths(&p));
            std::env::join_paths(paths)?
        }
        None => shims.clone().into_os_string(),
    };

    let sdkroot = std::env::var_os("SDKROOT");
    if sdkroot.is_none() {
        eprintln!(
            "note: SDKROOT is not set. Pure egui+wgpu apps usually build without it; \
             set it to your xtool iOS SDK if a C dependency invokes xcrun."
        );
    }

    println!("cargo build --target {triple} --release");
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target")
        .arg(triple)
        .arg("--release")
        .env("PATH", &path);
    if let Some(sdk) = &sdkroot {
        cmd.env("SDKROOT", sdk);
    }
    let status = cmd.status().context("running cargo build")?;
    if !status.success() {
        bail!("cargo build failed");
    }

    // Resolve the real target dir (handles workspaces and CARGO_TARGET_DIR) via cargo metadata.
    let target_dir = resolve_target_dir(&manifest)?;

    if let Some(assets) = &args.assets {
        bundle_assets(assets, &root.join("Sources"))?;
    }

    println!("staticlib: {}/{triple}/release/", target_dir.display());
    Ok(target_dir)
}

fn bundle_assets(src: &Path, sources_dir: &Path) -> Result<()> {
    let dst = sources_dir.join("assets");
    fs::create_dir_all(&dst)?;
    // rsync -aL dereferences symlinks (installd rejects symlinks inside .app bundles).
    let status = Command::new("rsync")
        .args(["-aL", "--delete"])
        .arg(format!("{}/", src.display()))
        .arg(format!("{}/", dst.display()))
        .status()
        .context("running rsync for --assets")?;
    if !status.success() {
        bail!("rsync of assets failed");
    }
    println!("assets bundled into {} (add a .copy(\"assets\") resource to Package.swift)", dst.display());
    Ok(())
}

fn cmd_run(args: &BuildArgs) -> Result<()> {
    let target_dir = cmd_build(args)?;
    let root = std::env::current_dir()?;

    let shims = prepare_shims()?;
    let path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut paths = vec![shims];
            paths.extend(std::env::split_paths(&p));
            std::env::join_paths(paths)?
        }
        None => shims.into_os_string(),
    };

    println!("xtool dev");
    let mut cmd = Command::new("xtool");
    cmd.arg("dev")
        .current_dir(&root)
        .env("PATH", &path)
        .env("EGUI_IOS_RUST_TARGET_DIR", &target_dir);
    if let Some(sdk) = std::env::var_os("SDKROOT") {
        cmd.env("SDKROOT", sdk);
    }
    let status = cmd.status().context("running xtool dev")?;
    if !status.success() {
        bail!("xtool dev failed");
    }
    Ok(())
}
