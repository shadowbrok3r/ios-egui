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
