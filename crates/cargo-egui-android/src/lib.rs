//! Scaffold, cross-compile, and run native egui Android apps.
//!
//! Wraps `cargo-apk2` (gradle-free APK packaging + self-signed debug keystore) and `adb`. Sets up
//! ANDROID_HOME / ANDROID_NDK_HOME / JAVA_HOME from a standard `~/Android/Sdk` layout so the user
//! doesn't have to. No signing certificate treadmill — a debug keystore is auto-generated.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;

const TPL_CARGO: &str = include_str!("../template/Cargo.toml.tpl");
const TPL_LIB: &str = include_str!("../template/lib.rs.tpl");
const TPL_GITIGNORE: &str = include_str!("../template/gitignore.tpl");

#[derive(Args, Clone)]
pub struct BuildArgs {
    /// Build the release profile.
    #[arg(long)]
    pub release: bool,
}

fn ident(name: &str) -> String {
    name.replace(['-', ' '], "_")
}

fn pkg_segment(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

pub fn cmd_new(name: &str, package_id: Option<String>, display_name: Option<String>) -> Result<()> {
    let root = PathBuf::from(name);
    if root.exists() {
        bail!("{} already exists", root.display());
    }
    let project = ident(name);
    let display = display_name.unwrap_or_else(|| name.to_string());
    let package = package_id.unwrap_or_else(|| format!("com.example.{}", pkg_segment(name)));

    let render = |t: &str| {
        t.replace("{{project_name}}", &project)
            .replace("{{display_name}}", &display)
            .replace("{{package_id}}", &package)
    };

    write(&root.join("Cargo.toml"), &render(TPL_CARGO))?;
    write(&root.join("src/lib.rs"), &render(TPL_LIB))?;
    write(&root.join(".gitignore"), TPL_GITIGNORE)?;

    println!("Created {} ({project})", root.display());
    println!("Next:\n  cd {name}\n  # edit src/lib.rs\n  cargo egui-android run");
    Ok(())
}

/// Cross-compile + package the APK.
pub fn cmd_build(args: &BuildArgs) -> Result<()> {
    cmd_apk("build", args)
}

/// Build, install (adb), and launch on a connected device/emulator.
pub fn cmd_run(args: &BuildArgs) -> Result<()> {
    cmd_apk("run", args)
}

/// Locate the Android SDK, NDK, and a JDK 17+; export them for the child process.
fn env_for(cmd: &mut Command) -> Result<()> {
    let home = std::env::var("HOME").unwrap_or_default();
    let sdk = std::env::var_os("ANDROID_HOME")
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join("Android/Sdk"));
    if !sdk.exists() {
        bail!(
            "Android SDK not found at {}. Set ANDROID_HOME or install the SDK.",
            sdk.display()
        );
    }
    cmd.env("ANDROID_HOME", &sdk);

    let ndk = std::env::var_os("ANDROID_NDK_HOME")
        .map(PathBuf::from)
        .or_else(|| newest_dir(&sdk.join("ndk")));
    match ndk {
        Some(ndk) => {
            cmd.env("ANDROID_NDK_HOME", &ndk);
        }
        None => bail!("No NDK under {}/ndk. Install one via sdkmanager.", sdk.display()),
    }

    let java_home = std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .or_else(newest_jdk);
    if let Some(jh) = &java_home {
        cmd.env("JAVA_HOME", jh);
    }

    // cargo-apk2 runs d8/aapt2 via the `java` on PATH (d8 needs JDK 11+, not the common system
    // JDK 8), and `adb` for `run` — so prepend the chosen JDK's bin and platform-tools.
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(jh) = &java_home {
        paths.push(jh.join("bin"));
    }
    paths.push(sdk.join("platform-tools"));
    if let Some(orig) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&orig));
    }
    cmd.env("PATH", std::env::join_paths(paths)?);
    Ok(())
}

/// Highest-versioned subdirectory of `dir` (lexicographic; version dirs sort correctly enough).
fn newest_dir(dir: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    entries.sort();
    entries.pop()
}

/// Pick a JDK suitable for the Android build tools. `d8`/`r8` (build-tools 35) needs JDK 11+ to
/// RUN but rejects bytecode newer than ~JDK 21, so prefer a JDK in 17..=21 over a newer one.
fn newest_jdk() -> Option<PathBuf> {
    let mut cands: Vec<(u32, PathBuf)> = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    // Userspace JDKs (e.g. a Temurin 17 extracted to ~/jdk17).
    for (v, name) in [(17u32, "jdk17"), (21, "jdk21")] {
        let p = PathBuf::from(&home).join(name);
        if p.join("bin/javac").exists() {
            cands.push((v, p));
        }
    }
    // System JDKs (Manjaro/Arch: /usr/lib/jvm/java-<N>-openjdk).
    if let Ok(rd) = fs::read_dir("/usr/lib/jvm") {
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if let Some(v) = name
                .strip_prefix("java-")
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<u32>().ok())
            {
                if p.join("bin/javac").exists() {
                    cands.push((v, p));
                }
            }
        }
    }
    // Highest in 17..=21, else lowest >= 17 (lower bytecode is safer for d8).
    cands
        .iter()
        .filter(|(v, _)| (17..=21).contains(v))
        .max_by_key(|(v, _)| *v)
        .or_else(|| cands.iter().filter(|(v, _)| *v >= 17).min_by_key(|(v, _)| *v))
        .map(|(_, p)| p.clone())
}

fn cmd_apk(sub: &str, args: &BuildArgs) -> Result<()> {
    if !PathBuf::from("Cargo.toml").exists() {
        bail!("run from an egui-android app directory (no Cargo.toml here)");
    }
    let mut cmd = Command::new("cargo");
    cmd.arg("apk2").arg(sub).arg("--target").arg("aarch64-linux-android");
    if args.release {
        cmd.arg("--release");
    }
    env_for(&mut cmd)?;
    println!("cargo apk2 {sub} --target aarch64-linux-android{}", if args.release { " --release" } else { "" });
    let status = cmd.status().context("running cargo apk2 (is cargo-apk2 installed?)")?;
    if !status.success() {
        bail!("cargo apk2 {sub} failed");
    }
    Ok(())
}
