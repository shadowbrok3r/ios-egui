//! Emits `OUT_DIR/library.rs` listing the desktop's board/component JSON as
//! `include_str!` entries, so the plugin carries a library with no filesystem.

use std::path::{Path, PathBuf};

const ASSETS: &str = "../../../../EmbeddedApps/wirelab/assets";

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let assets = std::fs::canonicalize(manifest.join(ASSETS)).unwrap_or_else(|e| {
        panic!("wirelab assets not found at {ASSETS} (relative to the plugin): {e}")
    });
    let mut src = String::new();
    for (name, sub) in [("BOARDS", "boards"), ("DEFS", "components")] {
        let dir = assets.join(sub);
        println!("cargo:rerun-if-changed={}", dir.display());
        src.push_str(&format!("pub static {name}: &[&str] = &[\n"));
        for path in json_files(&dir) {
            println!("cargo:rerun-if-changed={}", path.display());
            src.push_str(&format!("    include_str!(r\"{}\"),\n", path.display()));
        }
        src.push_str("];\n");
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("library.rs");
    std::fs::write(&out, src).unwrap();
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    if files.is_empty() {
        panic!("no *.json under {}", dir.display());
    }
    files
}
