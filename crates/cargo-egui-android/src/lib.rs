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
const TPL_JAVA_ACTIVITY: &str =
    include_str!("../../egui-android/java/com/github/egui_mobile/EguiNativeActivity.java");
const TPL_JAVA_BRIDGE: &str =
    include_str!("../../egui-android/java/com/github/egui_mobile/EguiImeBridge.java");

#[derive(Args, Clone)]
pub struct BuildArgs {
    /// Build the release profile.
    #[arg(long)]
    pub release: bool,
    /// Cargo features to enable on the app crate (e.g. `tls`).
    #[arg(long, value_delimiter = ',')]
    pub features: Vec<String>,
}

#[derive(Args, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    /// Connect over wireless adb before install/run (`host` or `host:port`; default port 5555).
    #[arg(long, value_name = "HOST[:PORT]")]
    pub tcp: Option<String>,
}

#[derive(Args, Clone)]
pub struct LogcatArgs {
    /// Connect over wireless adb first (`host` or `host:port`; default port 5555).
    #[arg(long, value_name = "HOST[:PORT]")]
    pub tcp: Option<String>,
    /// Log tags to allowlist (repeatable); default streams the app + QNN self-test tags.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
    /// Clear the device log buffer before streaming.
    #[arg(long)]
    pub clear: bool,
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
    write(
        &root.join("java/com/github/egui_mobile/EguiNativeActivity.java"),
        TPL_JAVA_ACTIVITY,
    )?;
    write(
        &root.join("java/com/github/egui_mobile/EguiImeBridge.java"),
        TPL_JAVA_BRIDGE,
    )?;

    println!("Created {} ({project})", root.display());
    println!("Next:\n  cd {name}\n  # edit src/lib.rs\n  cargo egui-android run");
    Ok(())
}

/// Cross-compile + package the APK.
pub fn cmd_build(args: &BuildArgs) -> Result<()> {
    cmd_apk("build", args, None)
}

/// Build, install (adb), and launch on a connected device/emulator.
pub fn cmd_run(args: &RunArgs) -> Result<()> {
    let device = if let Some(tcp) = &args.tcp {
        let serial = normalize_tcp_serial(tcp);
        adb_connect(&serial)?;
        Some(serial)
    } else {
        None
    };
    cmd_apk("run", &args.build, device.as_deref())
}

/// `adb connect host[:port]` (default port 5555) and verify the device is online.
pub fn cmd_adb_connect(host: &str) -> Result<()> {
    let serial = normalize_tcp_serial(host);
    adb_connect(&serial)?;
    println!("Connected to {serial}");
    Ok(())
}

/// Default logcat tags: the comfyui in-app logger plus the QNN/local-sd self-test targets.
const DEFAULT_LOGCAT_TAGS: &[&str] = &["comfyui", "local_sd", "qnn_rs", "egui-android"];

/// Stream device logs (blocks until interrupted) with the resolved SDK env, allowlisting the
/// app + self-test tags so a diagnostic printed via `log`/`android_logger` is easy to read back.
pub fn cmd_logcat(args: &LogcatArgs) -> Result<()> {
    let env = resolve_android_env()?;
    let adb = adb_path(&env);
    let serial = if let Some(tcp) = &args.tcp {
        let serial = normalize_tcp_serial(tcp);
        adb_connect(&serial)?;
        Some(serial)
    } else {
        None
    };
    if args.clear {
        let mut clear = Command::new(&adb);
        if let Some(s) = &serial {
            clear.arg("-s").arg(s);
        }
        clear.arg("logcat").arg("-c").env("PATH", &env.path);
        let _ = clear.status();
    }
    let specs: Vec<String> = if args.tags.is_empty() {
        DEFAULT_LOGCAT_TAGS.iter().map(|t| format!("{t}:V")).collect()
    } else {
        args.tags.iter().map(|t| format!("{t}:V")).collect()
    };
    let mut cmd = Command::new(&adb);
    if let Some(s) = &serial {
        cmd.arg("-s").arg(s);
    }
    cmd.arg("logcat").arg("-s");
    for spec in &specs {
        cmd.arg(spec);
    }
    cmd.env("PATH", &env.path);
    println!("adb logcat -s {}", specs.join(" "));
    let status = cmd.status().context("running adb logcat (is platform-tools installed?)")?;
    if !status.success() {
        bail!("adb logcat exited with failure");
    }
    Ok(())
}

/// Append `:5555` when the host has no port.
pub fn normalize_tcp_serial(host: &str) -> String {
    let host = host.trim();
    if host.is_empty() {
        return "localhost:5555".into();
    }
    // IPv6 in brackets: `[::1]:5555` or bare `[::1]`.
    if host.starts_with('[') {
        if host.contains("]:") {
            return host.to_string();
        }
        return format!("{host}:5555");
    }
    // host:port — only treat the last `:` as port when the suffix is numeric.
    if let Some((h, p)) = host.rsplit_once(':') {
        if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            return host.to_string();
        }
    }
    format!("{host}:5555")
}

fn adb_path(env: &AndroidEnv) -> PathBuf {
    let candidate = env.sdk.join("platform-tools/adb");
    if candidate.is_file() {
        candidate
    } else {
        PathBuf::from("adb")
    }
}

fn adb_connect(serial: &str) -> Result<()> {
    let env = resolve_android_env()?;
    let adb = adb_path(&env);
    println!("adb connect {serial}");
    let mut connect = Command::new(&adb);
    connect.arg("connect").arg(serial);
    connect.env("PATH", &env.path);
    let out = connect.output().context("running adb connect (is platform-tools installed?)")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    if !out.status.success() {
        bail!("adb connect {serial} failed: {}", combined.trim());
    }
    // adb often exits 0 with "failed to connect" / "unable to connect" in stdout.
    let lower = combined.to_ascii_lowercase();
    if lower.contains("failed to connect") || lower.contains("unable to connect") {
        bail!("adb connect {serial} failed: {}", combined.trim());
    }
    if !combined.trim().is_empty() {
        print!("{}", combined);
        if !combined.ends_with('\n') {
            println!();
        }
    }
    verify_adb_device(&adb, &env.path, serial)?;
    Ok(())
}

fn verify_adb_device(adb: &Path, path: &std::ffi::OsStr, serial: &str) -> Result<()> {
    let mut devices = Command::new(adb);
    devices.arg("devices");
    devices.env("PATH", path);
    let out = devices.output().context("running adb devices")?;
    if !out.status.success() {
        bail!("adb devices failed");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let online = text.lines().any(|line| {
        let mut parts = line.split_whitespace();
        matches!((parts.next(), parts.next()), (Some(s), Some("device")) if s == serial)
    });
    if !online {
        bail!(
            "device {serial} not listed as `device` in `adb devices`.\n\
             One-time phone setup: enable Wireless debugging (Android 11+) or\n\
             `adb tcpip 5555` once over USB, then retry."
        );
    }
    Ok(())
}

/// Print `export` lines for the same env `build`/`run` inject into cargo-apk2.
///
/// Use before bare `cargo apk2`: `eval "$(cargo egui-mobile env -a)"`.
pub fn cmd_env() -> Result<()> {
    let e = resolve_android_env()?;
    println!("export ANDROID_HOME={}", shell_quote(&e.sdk));
    println!("export ANDROID_NDK_HOME={}", shell_quote(&e.ndk));
    if let Some(jh) = &e.java_home {
        println!("export JAVA_HOME={}", shell_quote(jh));
    }
    if let Some(kh) = &e.kotlin_home {
        println!("export KOTLIN_HOME={}", shell_quote(kh));
    } else {
        eprintln!(
            "warning: kotlinc not found; install kotlin (e.g. `sudo pacman -S kotlin`) or set KOTLIN_HOME"
        );
    }
    println!("export PATH={}", shell_quote_os(&e.path));
    Ok(())
}

struct AndroidEnv {
    sdk: PathBuf,
    ndk: PathBuf,
    java_home: Option<PathBuf>,
    kotlin_home: Option<PathBuf>,
    path: std::ffi::OsString,
}

fn resolve_android_env() -> Result<AndroidEnv> {
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

    let ndk = std::env::var_os("ANDROID_NDK_HOME")
        .map(PathBuf::from)
        .or_else(|| newest_dir(&sdk.join("ndk")))
        .ok_or_else(|| anyhow::anyhow!("No NDK under {}/ndk. Install one via sdkmanager.", sdk.display()))?;

    let java_home = std::env::var_os("JAVA_HOME").map(PathBuf::from).or_else(newest_jdk);

    let kotlin_home = match std::env::var_os("KOTLIN_HOME").map(PathBuf::from) {
        Some(home) if kotlin_bin_exists(&home) => Some(home),
        _ => detect_kotlin_home(),
    };

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

    Ok(AndroidEnv {
        sdk,
        ndk,
        java_home,
        kotlin_home,
        path: std::env::join_paths(paths)?,
    })
}

/// Locate the Android SDK, NDK, and a JDK 17+; export them for the child process.
fn env_for(cmd: &mut Command) -> Result<()> {
    let e = resolve_android_env()?;
    cmd.env("ANDROID_HOME", &e.sdk);
    cmd.env("ANDROID_NDK_HOME", &e.ndk);
    if let Some(jh) = &e.java_home {
        cmd.env("JAVA_HOME", jh);
    }
    if let Some(kh) = &e.kotlin_home {
        cmd.env("KOTLIN_HOME", kh);
    }
    cmd.env("PATH", &e.path);
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    shell_quote_os(path.as_os_str())
}

fn shell_quote_os(value: &std::ffi::OsStr) -> String {
    let s = value.to_string_lossy();
    // Single-quote for POSIX `eval "$(…)"`; escape embedded quotes.
    format!("'{}'", s.replace('\'', "'\\''"))
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

fn kotlin_bin_exists(home: &Path) -> bool {
    let bin = home.join("bin");
    bin.join("kotlinc").exists() || bin.join("kotlinc.bat").exists()
}

/// Resolve KOTLIN_HOME so `$KOTLIN_HOME/bin/kotlinc` exists (what cargo-apk2 runs).
///
/// Distro layouts differ: JetBrains zips use `<home>/bin/kotlinc` + `<home>/lib`; Manjaro/Arch
/// put the launcher at `/usr/bin/kotlinc` and jars at `/usr/share/kotlin/lib`. For cargo-apk2,
/// only the `bin/kotlinc` path matters — the launcher script finds the jars itself.
fn detect_kotlin_home() -> Option<PathBuf> {
    let kotlinc = std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var).find_map(|dir| {
            let candidate = dir.join("kotlinc");
            if candidate.is_file() { Some(candidate) } else { None }
        })
    });
    if let Some(kotlinc) = kotlinc {
        let canonical = fs::canonicalize(&kotlinc).unwrap_or(kotlinc);
        // `/usr/bin/kotlinc` → `/usr`; `<sdk>/bin/kotlinc` → `<sdk>`.
        if let Some(home) = canonical.parent().and_then(|bin| bin.parent()) {
            if kotlin_bin_exists(home) {
                return Some(home.to_path_buf());
            }
        }
    }
    // Fallbacks when PATH is minimal (e.g. some IDE/task runners).
    for home in ["/usr", "/usr/local", "/opt/kotlin"] {
        let home = PathBuf::from(home);
        if kotlin_bin_exists(&home) {
            return Some(home);
        }
    }
    None
}

fn cmd_apk(sub: &str, args: &BuildArgs, device: Option<&str>) -> Result<()> {
    if !PathBuf::from("Cargo.toml").exists() {
        bail!("run from an egui-android app directory (no Cargo.toml here)");
    }
    let mut cmd = Command::new("cargo");
    cmd.arg("apk2").arg(sub).arg("--target").arg("aarch64-linux-android");
    if args.release {
        cmd.arg("--release");
    }
    let mut shown = String::new();
    if !args.features.is_empty() {
        let list = args.features.join(",");
        cmd.arg("--features").arg(&list);
        shown.push_str(" --features ");
        shown.push_str(&list);
    }
    if let Some(serial) = device {
        cmd.arg("--device").arg(serial);
        shown.push_str(" --device ");
        shown.push_str(serial);
    }
    env_for(&mut cmd)?;
    println!(
        "cargo apk2 {sub} --target aarch64-linux-android{}{shown}",
        if args.release { " --release" } else { "" }
    );
    let status = cmd.status().context("running cargo apk2 (is cargo-apk2 installed?)")?;
    if !status.success() {
        bail!("cargo apk2 {sub} failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::normalize_tcp_serial;

    #[test]
    fn tcp_serial_defaults_port() {
        assert_eq!(normalize_tcp_serial("192.168.1.20"), "192.168.1.20:5555");
        assert_eq!(normalize_tcp_serial("phone.local:5556"), "phone.local:5556");
        assert_eq!(normalize_tcp_serial("[::1]"), "[::1]:5555");
        assert_eq!(normalize_tcp_serial("[::1]:5556"), "[::1]:5556");
    }
}
