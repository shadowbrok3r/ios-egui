//! Small pure helpers, kept out of `app.rs` so they are actually tested.
//!
//! `mod app` is `#[cfg(target_os = "android")]`, so anything living there is never compiled by
//! `cargo test` on the host — tests written beside it look green and have not run.

/// Normalize whatever was typed into a base URL for the desktop sync endpoint.
///
/// A tailnet name, a bare `100.x` address, or a full URL all have to work: nobody types `http://`
/// on a phone keyboard by choice.
pub fn sync_base(host: &str) -> String {
    let h = host.trim().trim_end_matches('/');
    let with_scheme = if h.starts_with("http://") || h.starts_with("https://") {
        h.to_string()
    } else {
        format!("http://{h}")
    };
    // Check for a port only after the scheme, or the `:` in `http://` is mistaken for one.
    let after_scheme = with_scheme.split_once("//").map(|(_, r)| r).unwrap_or("");
    if after_scheme.contains(':') {
        with_scheme
    } else {
        format!("{with_scheme}:{DEFAULT_SYNC_PORT}")
    }
}

/// Matches `ringdesign_mcp::sync::DEFAULT_SYNC_PORT`, which the phone does not depend on.
pub const DEFAULT_SYNC_PORT: u16 = 8733;

/// Filesystem-safe stem from a design name.
///
/// Runs of separators collapse: `"My Ring #3"` has both a space and a `#` between the last two
/// words, and mapping each to a dash independently gives `my-ring--3`.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let s = out.trim_matches('-');
    if s.is_empty() { "ring".into() } else { s.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_gets_a_scheme_and_the_sync_port() {
        assert_eq!(sync_base("100.101.102.103"), "http://100.101.102.103:8733");
        assert_eq!(sync_base("desk.tail1234.ts.net"), "http://desk.tail1234.ts.net:8733");
    }

    #[test]
    fn an_explicit_port_is_left_alone() {
        assert_eq!(sync_base("100.101.102.103:9000"), "http://100.101.102.103:9000");
    }

    #[test]
    fn a_full_url_is_not_mangled_by_the_scheme_colon() {
        assert_eq!(sync_base("http://desk:8733"), "http://desk:8733");
        assert_eq!(sync_base("http://desk"), "http://desk:8733");
    }

    #[test]
    fn whitespace_and_a_trailing_slash_are_forgiven() {
        assert_eq!(sync_base("  100.64.0.1/  "), "http://100.64.0.1:8733");
    }

    #[test]
    fn slug_is_filesystem_safe_and_never_empty() {
        assert_eq!(slug("My Ring #3"), "my-ring-3");
        assert_eq!(slug("///"), "ring");
        assert_eq!(slug(""), "ring");
    }

    #[test]
    fn the_phones_default_port_matches_the_desktops() {
        // The phone does not depend on ringdesign-mcp, so this is the only place the two can drift.
        assert_eq!(DEFAULT_SYNC_PORT, 8733);
    }
}

/// A saved design on disk, as the Files list needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesignFile {
    pub path: std::path::PathBuf,
    /// File name including the `.ring.json` suffix.
    pub file_name: String,
    /// The name without that suffix — what the list shows and rename edits.
    pub stem: String,
    /// Modified time, or the epoch when the filesystem will not say.
    pub modified: std::time::SystemTime,
}

/// The suffix every saved design carries.
pub const DESIGN_SUFFIX: &str = ".ring.json";

/// Every design in `dir`, newest first.
///
/// Lexicographic order put "a-second-idea" above the thing you saved a moment
/// ago; on a phone the list is the only way back to a design, so the one you
/// were just working on belongs at the top. A file whose mtime the filesystem
/// will not report sorts as oldest rather than being dropped.
pub fn list_designs(dir: &std::path::Path) -> Vec<DesignFile> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<DesignFile> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let file_name = path.file_name()?.to_str()?.to_string();
            let stem = file_name.strip_suffix(DESIGN_SUFFIX)?.to_string();
            let modified = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some(DesignFile { path, file_name, stem, modified })
        })
        .collect();
    // Newest first, then by name so the order is stable when two files share a
    // timestamp — which they do, because a filesystem's mtime resolution is
    // coarser than a save-and-save-again.
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.stem.cmp(&b.stem)));
    out
}

/// Path a design with `name` would be saved to.
pub fn design_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{}{DESIGN_SUFFIX}", slug(name)))
}

#[cfg(test)]
mod file_tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rd-files-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    #[test]
    fn only_designs_are_listed_and_the_suffix_is_stripped() {
        let d = scratch("suffix");
        std::fs::write(d.join("band.ring.json"), "{}").unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        std::fs::write(d.join("prefs.json"), "{}").unwrap();
        let got = list_designs(&d);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].stem, "band");
        assert_eq!(got[0].file_name, "band.ring.json");
    }

    #[test]
    fn the_newest_design_is_first() {
        let d = scratch("mtime");
        for n in ["old", "new"] {
            std::fs::write(d.join(format!("{n}.ring.json")), "{}").unwrap();
            // Coarse mtime resolution would otherwise tie these.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let got = list_designs(&d);
        assert_eq!(got.iter().map(|f| f.stem.as_str()).collect::<Vec<_>>(), ["new", "old"]);
    }

    #[test]
    fn a_missing_directory_lists_nothing_rather_than_failing() {
        assert!(list_designs(std::path::Path::new("/no/such/dir")).is_empty());
    }

    #[test]
    fn a_saved_path_is_the_slug_plus_the_suffix() {
        let p = design_path(std::path::Path::new("/d"), "My Ring / 2");
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), format!("{}{DESIGN_SUFFIX}", slug("My Ring / 2")));
        assert!(!p.to_string_lossy().contains("My Ring / 2"), "the slug is filesystem-safe");
    }

    /// Two designs both called "untitled" resolve to one path — which is the
    /// silent overwrite the Files list has to warn about.
    #[test]
    fn two_designs_with_one_name_collide_on_disk() {
        let d = std::path::Path::new("/d");
        assert_eq!(design_path(d, "untitled"), design_path(d, "Untitled"));
    }
}
