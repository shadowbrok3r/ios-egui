//! Hosts the proxy passes through untouched — tunneled byte for byte, never MITM'd.
//!
//! Certificate-pinned apps break when their TLS is terminated, so anything that must keep working
//! goes here.

use std::sync::{Arc, RwLock};

#[derive(Clone, Default)]
pub struct ExclusionStore(Arc<RwLock<Vec<String>>>);

impl ExclusionStore {
    pub fn new(patterns: Vec<String>) -> Self {
        Self(Arc::new(RwLock::new(patterns)))
    }

    pub fn replace(&self, patterns: Vec<String>) {
        if let Ok(mut guard) = self.0.write() {
            *guard = patterns;
        }
    }

    pub fn contains(&self, host: &str) -> bool {
        self.0
            .read()
            .map(|patterns| patterns.iter().any(|pattern| matches(pattern, host)))
            .unwrap_or(false)
    }
}

/// Exact host, or `*.example.com` covering `example.com` and any subdomain.
fn matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
        None => pattern == host,
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn wildcard_covers_apex_and_subdomains() {
        assert!(matches("*.example.com", "example.com"));
        assert!(matches("*.example.com", "api.example.com"));
        assert!(!matches("*.example.com", "notexample.com"));
        assert!(!matches("example.com", "api.example.com"));
    }
}
