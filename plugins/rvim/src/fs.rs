//! Virtual filesystem persisted through the host `state.set`/`state.get` ops, mirrored
//! in memory so reads are free and native tests run without a host.

use std::collections::BTreeMap;

use egui_ios_plugin_sdk::{HostCallError, HostHandle};

/// State-store key holding the postcard-encoded `Vec<String>` of file names.
const INDEX_KEY: &str = "index";

/// State-store key for one file's raw utf8 content.
fn file_key(name: &str) -> String {
    format!("file:{name}")
}

pub struct Vfs {
    host: HostHandle,
    files: BTreeMap<String, String>,
}

impl Vfs {
    /// Load all files from host state; seeds the sample project on first run.
    pub fn load() -> Self {
        let host = HostHandle;
        let index = host.state_get(INDEX_KEY);
        Self::from_index_response(host, index)
    }

    /// Build the vfs from an index fetch result: present → read files, missing → seed,
    /// host failure → empty in-memory store.
    fn from_index_response(
        host: HostHandle,
        index: Result<Option<Vec<u8>>, HostCallError>,
    ) -> Self {
        let mut vfs = Vfs { host, files: BTreeMap::new() };
        match index {
            Ok(Some(bytes)) => {
                let names: Vec<String> = postcard::from_bytes(&bytes).unwrap_or_default();
                for name in names {
                    if let Ok(Some(content)) = host.state_get(&file_key(&name)) {
                        vfs.files.insert(name, String::from_utf8_lossy(&content).into_owned());
                    }
                }
            }
            Ok(None) => vfs.seed(),
            Err(_) => {}
        }
        vfs
    }

    /// Populate (and best-effort persist) the first-run sample project.
    fn seed(&mut self) {
        for (name, text) in SAMPLE_FILES {
            self.write(name, text);
        }
    }

    /// File names in sorted order.
    pub fn list(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    /// Borrowed file names in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// The i-th file name in sorted order.
    pub fn name_at(&self, i: usize) -> Option<&str> {
        self.files.keys().nth(i).map(String::as_str)
    }

    pub fn read(&self, name: &str) -> Option<String> {
        self.files.get(name).cloned()
    }

    /// Write a file to memory and persist it (best-effort) to host state.
    pub fn write(&mut self, name: &str, text: &str) {
        self.files.insert(name.to_string(), text.to_string());
        let _ = self.host.state_set(&file_key(name), text.as_bytes());
        self.persist_index();
    }

    /// Delete a file from memory and host state; returns false when it did not exist.
    pub fn remove(&mut self, name: &str) -> bool {
        if self.files.remove(name).is_none() {
            return false;
        }
        // No delete op exists; shrink the orphaned blob to empty.
        let _ = self.host.state_set(&file_key(name), &[]);
        self.persist_index();
        true
    }

    #[allow(dead_code)]
    pub fn exists(&self, name: &str) -> bool {
        self.files.contains_key(name)
    }

    /// Best-effort write of the file-name index to host state.
    fn persist_index(&self) {
        let names: Vec<&String> = self.files.keys().collect();
        if let Ok(bytes) = postcard::to_stdvec(&names) {
            let _ = self.host.state_set(INDEX_KEY, &bytes);
        }
    }
}

/// Sample project written on first run (missing index).
const SAMPLE_FILES: &[(&str, &str)] = &[
    ("main.rs", SAMPLE_MAIN),
    ("lib.rs", SAMPLE_LIB),
    ("notes.md", SAMPLE_NOTES),
];

const SAMPLE_MAIN: &str = r#"//! Sample project seeded by rvim on first run.
//!
//! /* Block comments /* nest */ in rust — the highlighter tracks depth. */

use std::env;

#[derive(Debug, Clone)]
struct Item {
    text: String,
    done: bool,
}

fn parse(args: &[String]) -> Vec<Item> {
    let mut items = Vec::new();
    for arg in args {
        let done = arg.starts_with('+');
        let text = arg.trim_start_matches('+').to_string();
        items.push(Item { text, done });
    }
    items
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let items = parse(&args);
    for (i, item) in items.iter().enumerate() {
        let mark = match item.done {
            true => "[x]",
            false => "[ ]",
        };
        println!("{:>2}. {mark} {}", i + 1, item.text);
    }
    println!("{} items total", items.len());
}
"#;

const SAMPLE_LIB: &str = r#"//! Tiny geometry helpers for the sample project.

pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }

    pub fn dist(&self, other: &Point) -> f64 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }
}

pub fn largest<T: PartialOrd + Copy>(items: &[T]) -> Option<T> {
    let mut best = *items.first()?;
    for &it in &items[1..] {
        if it > best {
            best = it;
        }
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_works() {
        let d = Point::new(0.0, 0.0).dist(&Point::new(3.0, 4.0));
        assert!((d - 5.0).abs() < 1e-9);
    }
}
"#;

const SAMPLE_NOTES: &str = "# notes

rvim keeps these files in the plugin state store.

- `:e <name>` opens or creates a file
- `Space f f` (or Ctrl+p) fuzzy-finds across the project
- `:help` shows the full cheatsheet
- markdown gets no rust highlighting, as it should
";

#[cfg(test)]
mod tests {
    use super::*;

    fn no_host_vfs() -> Vfs {
        Vfs::from_index_response(HostHandle, Err(HostCallError::Failed("no host".into())))
    }

    #[test]
    fn write_read_roundtrip() {
        let mut v = no_host_vfs();
        v.write("a.rs", "fn a() {}");
        assert_eq!(v.read("a.rs").as_deref(), Some("fn a() {}"));
        assert!(v.exists("a.rs"));
        assert!(!v.exists("b.rs"));
        assert_eq!(v.read("b.rs"), None);
    }

    #[test]
    fn overwrite_replaces_content() {
        let mut v = no_host_vfs();
        v.write("a.rs", "one");
        v.write("a.rs", "two");
        assert_eq!(v.read("a.rs").as_deref(), Some("two"));
        assert_eq!(v.list().len(), 1);
    }

    #[test]
    fn remove_deletes_and_reports_missing() {
        let mut v = no_host_vfs();
        v.write("a.rs", "x");
        assert!(v.remove("a.rs"));
        assert!(!v.exists("a.rs"));
        assert!(!v.remove("a.rs"));
        assert!(!v.remove("never-existed"));
    }

    #[test]
    fn list_is_sorted() {
        let mut v = no_host_vfs();
        v.write("zeta.rs", "");
        v.write("alpha.rs", "");
        v.write("mid.md", "");
        assert_eq!(v.list(), vec!["alpha.rs", "mid.md", "zeta.rs"]);
    }

    #[test]
    fn seed_when_index_missing() {
        let v = Vfs::from_index_response(HostHandle, Ok(None));
        assert_eq!(v.list(), vec!["lib.rs", "main.rs", "notes.md"]);
        assert!(v.read("main.rs").unwrap().contains("fn main()"));
        assert!(v.read("lib.rs").unwrap().contains("#[cfg(test)]"));
        assert!(v.read("notes.md").unwrap().starts_with("# notes"));
    }

    #[test]
    fn no_seed_when_index_present() {
        let bytes = postcard::to_stdvec(&vec!["a.rs".to_string()]).unwrap();
        let v = Vfs::from_index_response(HostHandle, Ok(Some(bytes)));
        assert!(!v.exists("main.rs"));
        // File reads fail without a host, so the entry is skipped.
        assert!(v.list().is_empty());
    }

    #[test]
    fn no_seed_when_host_unavailable() {
        assert!(no_host_vfs().list().is_empty());
    }

    #[test]
    fn no_seed_on_corrupt_index() {
        let v = Vfs::from_index_response(HostHandle, Ok(Some(vec![0xff, 0xff, 0xff, 0xff])));
        assert!(v.list().is_empty());
    }

    #[test]
    fn native_load_falls_back_to_empty() {
        assert!(Vfs::load().list().is_empty());
    }

    #[test]
    fn unicode_content_roundtrip() {
        let mut v = no_host_vfs();
        v.write("uni.rs", "let é = \"中文\";");
        assert_eq!(v.read("uni.rs").as_deref(), Some("let é = \"中文\";"));
    }
}
