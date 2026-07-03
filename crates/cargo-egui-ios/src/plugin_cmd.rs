//! `cargo egui-ios plugin` — scaffold, build, watch, and serve WASM UI plugins.

use std::collections::HashMap;
use std::io::{Read, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

const WASM_TRIPLE: &str = "wasm32-wasip1";

const TPL_CARGO: &str = include_str!("../../../template-plugin/Cargo.toml");
const TPL_LIB: &str = include_str!("../../../template-plugin/lib.rs");
const TPL_MANIFEST: &str = include_str!("../../../template-plugin/manifest.toml");
const TPL_GITIGNORE: &str = include_str!("../../../template-plugin/gitignore");

#[derive(Subcommand)]
pub enum PluginCmd {
    /// Scaffold a new plugin crate in a new directory.
    New {
        /// Plugin / directory name (e.g. my-widget).
        name: String,
        /// Plugin id (default: com.example.<name>).
        #[arg(long)]
        id: Option<String>,
        /// Display name shown in the plugin manager (default: <name>).
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Build to wasm32-wasip1 and stage `<out>/<id>/{plugin.wasm, manifest.toml}`.
    Build(PluginBuildArgs),
    /// Build, watch, and serve plugins over HTTP for hot reload on device.
    Serve(PluginServeArgs),
}

#[derive(Args, Clone)]
pub struct PluginBuildArgs {
    /// Plugin crate directory.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Build without --release.
    #[arg(long)]
    debug: bool,
    /// Staging directory (default: `<path>/dist`).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args)]
pub struct PluginServeArgs {
    /// Plugin crate directories (or a directory of plugins) to build, watch, and serve.
    /// A directory without a manifest.toml is expanded to every plugin crate it contains,
    /// so pointing at the `plugins/` folder — or running from the repo root — serves them all.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// Listen address.
    #[arg(long, default_value = "0.0.0.0:7878")]
    addr: String,
    /// Build without --release.
    #[arg(long)]
    debug: bool,
}

pub fn run(cmd: PluginCmd) -> Result<()> {
    match cmd {
        PluginCmd::New { name, id, display_name } => cmd_new(&name, id, display_name),
        PluginCmd::Build(args) => {
            let built = build_plugin(&args.path, args.debug)?;
            let out = args.out.unwrap_or_else(|| args.path.join("dist"));
            let staged = stage(&built, &out)?;
            println!("staged {}", staged.display());
            Ok(())
        }
        PluginCmd::Serve(args) => cmd_serve(&args),
    }
}

fn cmd_new(name: &str, id: Option<String>, display_name: Option<String>) -> Result<()> {
    let root = PathBuf::from(name);
    if root.exists() {
        bail!("{} already exists", root.display());
    }
    let project_name = name.replace(['-', ' '], "_");
    let display_name = display_name.unwrap_or_else(|| name.to_string());
    let seg: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let plugin_id = id.unwrap_or_else(|| format!("com.example.{seg}"));

    let render = |tpl: &str| {
        tpl.replace("{{project_name}}", &project_name)
            .replace("{{display_name}}", &display_name)
            .replace("{{plugin_id}}", &plugin_id)
    };
    let write = |rel: &str, contents: String| -> Result<()> {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
    };

    write("Cargo.toml", render(TPL_CARGO))?;
    write("src/lib.rs", render(TPL_LIB))?;
    write("manifest.toml", render(TPL_MANIFEST))?;
    write(".gitignore", TPL_GITIGNORE.to_string())?;

    println!("Created plugin {} ({plugin_id})", root.display());
    println!("Next:");
    println!("  cd {name}");
    println!("  # edit src/lib.rs");
    println!("  cargo egui-ios plugin serve   # then connect from the app's plugin manager");
    Ok(())
}

struct Built {
    id: String,
    manifest: String,
    wasm: Vec<u8>,
}

fn build_plugin(dir: &Path, debug: bool) -> Result<Built> {
    let manifest_path = dir.join("manifest.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest_val: toml::Value = toml::from_str(&manifest).context("parsing manifest.toml")?;
    let id = manifest_val
        .get("id")
        .and_then(|v| v.as_str())
        .context("manifest.toml: missing `id`")?
        .to_owned();

    let cargo_toml_path = dir.join("Cargo.toml");
    let cargo_toml: toml::Value = toml::from_str(
        &std::fs::read_to_string(&cargo_toml_path)
            .with_context(|| format!("reading {}", cargo_toml_path.display()))?,
    )?;
    let crate_name = cargo_toml
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .context("Cargo.toml: missing package.name")?
        .replace('-', "_");

    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&cargo_toml_path)
        .arg("--target")
        .arg(WASM_TRIPLE);
    if !debug {
        cmd.arg("--release");
    }
    let status = cmd.status().context("running cargo build")?;
    if !status.success() {
        bail!("cargo build failed for {}", dir.display());
    }

    let target_dir = crate::resolve_target_dir(&cargo_toml_path)?;
    let profile = if debug { "debug" } else { "release" };
    let wasm_path = target_dir
        .join(WASM_TRIPLE)
        .join(profile)
        .join(format!("{crate_name}.wasm"));
    let wasm = std::fs::read(&wasm_path)
        .with_context(|| format!("reading {}", wasm_path.display()))?;
    Ok(Built { id, manifest, wasm })
}

fn stage(built: &Built, out: &Path) -> Result<PathBuf> {
    let dir = out.join(&built.id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("plugin.wasm"), &built.wasm)?;
    std::fs::write(dir.join("manifest.toml"), &built.manifest)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------------------
// Dev server
// ---------------------------------------------------------------------------------------

struct Served {
    manifest: String,
    wasm: Vec<u8>,
    hash: String,
}

type ServeState = Arc<Mutex<HashMap<String, Served>>>;

/// Expand each input path into concrete plugin crate directories (those with a `manifest.toml`).
/// A path that is itself a plugin is used as-is; otherwise its immediate subdirectories — or a
/// nested `plugins/` directory — are scanned, so pointing at a folder of plugins (or the repo
/// root) serves them all. Duplicates are removed.
fn expand_plugin_dirs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let push = |p: PathBuf, out: &mut Vec<PathBuf>| {
        if !out.iter().any(|q| q == &p) {
            out.push(p);
        }
    };
    for path in paths {
        if path.join("manifest.toml").is_file() {
            push(path.clone(), &mut out);
            continue;
        }
        let mut found = scan_plugin_children(path);
        if found.is_empty() && path.join("plugins").is_dir() {
            found = scan_plugin_children(&path.join("plugins"));
        }
        if found.is_empty() {
            eprintln!("no plugin crates found under {}", path.display());
        }
        for f in found {
            push(f, &mut out);
        }
    }
    out
}

/// Immediate subdirectories of `dir` that are plugin crates (contain a `manifest.toml`), sorted.
fn scan_plugin_children(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            if child.is_dir() && child.join("manifest.toml").is_file() {
                found.push(child);
            }
        }
    }
    found
}

fn cmd_serve(args: &PluginServeArgs) -> Result<()> {
    let paths = expand_plugin_dirs(&args.paths);
    if paths.is_empty() {
        bail!(
            "no plugin crates (dirs with a manifest.toml) found under {:?}",
            args.paths
        );
    }
    let names: Vec<&str> = paths
        .iter()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
        .collect();
    println!("serving {} plugin(s): {}", paths.len(), names.join(", "));

    let state: ServeState = Arc::new(Mutex::new(HashMap::new()));

    for path in &paths {
        match build_plugin(path, args.debug) {
            Ok(b) => insert(&state, b),
            Err(e) => eprintln!("initial build failed for {}: {e:#}", path.display()),
        }
    }

    {
        let debug = args.debug;
        let state = Arc::clone(&state);
        std::thread::spawn(move || watch_loop(&paths, debug, &state));
    }

    let listener = TcpListener::bind(&args.addr)
        .with_context(|| format!("binding {}", args.addr))?;
    println!("serving plugins at http://{}", args.addr);
    println!("connect from the app's plugin manager (dev server field), or:");
    println!("  curl http://127.0.0.1:{}/plugins.json", args.addr.rsplit(':').next().unwrap_or("7878"));
    // Bound concurrent handlers so a flood of connections can't spawn unbounded threads.
    const MAX_CONNS: usize = 32;
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONNS {
            drop(stream); // Shed load; the client retries on the next poll.
            continue;
        }
        active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let state = Arc::clone(&state);
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            let _ = handle_client(stream, &state);
            active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
    Ok(())
}

fn insert(state: &ServeState, built: Built) {
    let hash = format!("{:016x}", fnv1a64(&built.wasm));
    println!("plugin {} ready ({} KiB, {hash})", built.id, built.wasm.len() / 1024);
    state.lock().unwrap().insert(
        built.id.clone(),
        Served { manifest: built.manifest, wasm: built.wasm, hash },
    );
}

fn watch_loop(paths: &[PathBuf], debug: bool, state: &ServeState) {
    let mut stamps: HashMap<PathBuf, u64> = HashMap::new();
    for p in paths {
        stamps.insert(p.clone(), tree_stamp(p));
    }
    loop {
        std::thread::sleep(Duration::from_millis(700));
        for p in paths {
            let stamp = tree_stamp(p);
            if stamps.get(p) != Some(&stamp) {
                stamps.insert(p.clone(), stamp);
                println!("change detected in {}, rebuilding…", p.display());
                match build_plugin(p, debug) {
                    Ok(b) => insert(state, b),
                    Err(e) => eprintln!("rebuild failed: {e:#}"),
                }
            }
        }
    }
}

/// Cheap change stamp: fold mtimes+sizes of `src/`, `Cargo.toml`, `manifest.toml`.
fn tree_stamp(dir: &Path) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    fn walk(p: &Path, mix: &mut impl FnMut(&[u8])) {
        if let Ok(meta) = std::fs::metadata(p) {
            if let Ok(t) = meta.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    mix(&d.as_nanos().to_le_bytes());
                }
            }
            mix(&meta.len().to_le_bytes());
            if meta.is_dir() {
                if let Ok(entries) = std::fs::read_dir(p) {
                    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
                    paths.sort();
                    for child in paths {
                        mix(child.as_os_str().as_encoded_bytes());
                        walk(&child, mix);
                    }
                }
            } else if let Ok(bytes) = std::fs::read(p) {
                // Hash contents, not just mtime+size — catches same-size edits within the
                // filesystem's mtime granularity. Watched files are small (src, manifests).
                mix(&bytes);
            }
        }
    }
    walk(&dir.join("src"), &mut mix);
    walk(&dir.join("Cargo.toml"), &mut mix);
    walk(&dir.join("manifest.toml"), &mut mix);
    h
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn handle_client(mut stream: TcpStream, state: &ServeState) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0u8; 4096];
    let mut head = Vec::new();
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > 16 * 1024 {
            break;
        }
    }
    let request = String::from_utf8_lossy(&head);
    let path = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();

    let state = state.lock().unwrap();
    if path == "/plugins.json" {
        let list: Vec<serde_json::Value> = state
            .iter()
            .map(|(id, s)| {
                serde_json::json!({
                    "id": id,
                    "hash": s.hash,
                    "wasm": format!("/plugins/{id}/plugin.wasm"),
                    "manifest": format!("/plugins/{id}/manifest.toml"),
                })
            })
            .collect();
        return respond(&mut stream, 200, "application/json", serde_json::to_string(&list)?.as_bytes());
    }
    if let Some(rest) = path.strip_prefix("/plugins/") {
        if let Some((id, file)) = rest.split_once('/') {
            if let Some(s) = state.get(id) {
                return match file {
                    "plugin.wasm" => respond(&mut stream, 200, "application/wasm", &s.wasm),
                    "manifest.toml" => respond(&mut stream, 200, "text/plain", s.manifest.as_bytes()),
                    _ => respond(&mut stream, 404, "text/plain", b"not found"),
                };
            }
        }
    }
    respond(&mut stream, 404, "text/plain", b"not found")
}

fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> Result<()> {
    let reason = if code == 200 { "OK" } else { "Not Found" };
    write!(
        stream,
        "HTTP/1.0 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}
