//! The adblock engine and the queries the proxy runs against it.
//!
//! privaxy owns the engine on a dedicated thread and talks to it over a crossbeam channel with a
//! oneshot reply per request. Here the engine sits behind an `RwLock` instead: matching only needs
//! `&self`, so requests match concurrently and cosmetic lookups no longer have to `block_on` a
//! channel round trip from inside the HTML rewriter.

use adblock::engine::Engine;
use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Default)]
pub struct CosmeticResult {
    pub hidden_selectors: Vec<String>,
    pub injected_script: Option<String>,
}

pub struct FilterEngine {
    engine: RwLock<Engine>,
    enabled: AtomicBool,
}

impl FilterEngine {
    pub fn empty() -> Self {
        Self {
            engine: RwLock::new(Engine::from_filter_set(FilterSet::new(true), true)),
            enabled: AtomicBool::new(true),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Rebuilds the engine from raw filter list text. Blocking and CPU heavy for large lists —
    /// call it from a blocking task, not the request path.
    pub fn replace(&self, filter_lists: &[String]) {
        let mut filter_set = FilterSet::new(true);
        for list in filter_lists {
            filter_set.add_filter_list(list, ParseOptions::default());
        }

        let engine = Engine::from_filter_set(filter_set, true);
        match self.engine.write() {
            Ok(mut guard) => *guard = engine,
            Err(poisoned) => *poisoned.into_inner() = engine,
        }
    }

    /// Returns the matching filter's text when the request should be blocked.
    pub fn check(&self, url: &str, source_url: &str, request_type: &str) -> Option<String> {
        if !self.is_enabled() {
            return None;
        }

        let request = Request::new(url, source_url, request_type).ok()?;
        let result = self.engine.read().ok()?.check_network_request(&request);

        result
            .matched
            .then(|| result.filter.unwrap_or_else(|| String::from("matched")))
    }

    /// Hostname-only match, used at CONNECT time where there is no path, referer or content type.
    /// Domain-anchored rules (`||host^`) make up the bulk of every list and match on host alone,
    /// which is what makes blocking work without terminating TLS.
    pub fn check_host(&self, host: &str) -> Option<String> {
        let url = format!("https://{host}/");
        self.check(&url, &url, "other")
    }

    pub fn cosmetic(&self, url: &str, ids: &[String], classes: &[String]) -> CosmeticResult {
        if !self.is_enabled() {
            return CosmeticResult::default();
        }

        let Ok(engine) = self.engine.read() else {
            return CosmeticResult::default();
        };

        let url_resources = engine.url_cosmetic_resources(url);

        let mut hidden_selectors = Vec::new();
        if !url_resources.generichide {
            hidden_selectors.extend(engine.hidden_class_id_selectors(
                classes,
                ids,
                &url_resources.exceptions,
            ));
        }
        hidden_selectors.extend(url_resources.hide_selectors);

        CosmeticResult {
            hidden_selectors,
            injected_script: (!url_resources.injected_script.is_empty())
                .then_some(url_resources.injected_script),
        }
    }
}

impl Default for FilterEngine {
    fn default() -> Self {
        Self::empty()
    }
}
