//! State shared between the proxy's Tokio threads and the egui frame loop.
//!
//! The UI reads this every frame, so nothing here may block for long: counters are atomics and the
//! request log is a bounded ring buffer behind a short-lived lock.

use crate::proxy::config::MitmMode;
use chrono::{DateTime, Local};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

const MAX_LOGGED_REQUESTS: usize = 400;
/// Entries that keep their bodies. Older ones stay in the log as headers and timings only —
/// otherwise a few video responses would own the app's whole heap.
const MAX_DETAILED_REQUESTS: usize = 60;
/// Kept per body. Enough for a document, a JSON payload or a beacon; a media stream is truncated.
const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Starting,
    Running { address: String },
    Failed(String),
}

impl Status {
    pub fn is_running(&self) -> bool {
        matches!(self, Status::Running { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FiltersStatus {
    Idle,
    Updating { completed: usize, total: usize },
    Ready { lists: usize },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Blocked { filter: String },
    /// Passed through byte for byte: hostname-only mode, or a never-intercept host.
    Tunneled,
    /// TLS was terminated, so the requests inside are logged separately as [`EventKind::Proxied`].
    /// A row with nothing under it means the client rejected the certificate.
    Intercepted,
    Proxied,
}

/// The prefix of a body kept for inspection, and how much went past it.
#[derive(Debug, Default, Clone)]
pub struct Body {
    pub bytes: Vec<u8>,
    /// Total bytes seen, kept or not.
    pub seen: u64,
    /// The prefix was dropped to keep the log bounded; `seen` still stands.
    pub evicted: bool,
}

impl Body {
    fn push(&mut self, chunk: &[u8]) {
        self.seen += chunk.len() as u64;
        if self.evicted {
            return;
        }
        let room = MAX_BODY_BYTES.saturating_sub(self.bytes.len());
        if room > 0 {
            self.bytes.extend_from_slice(&chunk[..room.min(chunk.len())]);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.seen == 0
    }

    /// More was sent than was kept.
    pub fn truncated(&self) -> bool {
        self.seen > self.bytes.len() as u64
    }
}

/// Everything the proxy saw of one exchange, filled in as it happens rather than at the end, so
/// the UI can open a request that is still streaming.
#[derive(Debug, Default)]
pub struct Exchange {
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<(String, String)>,
    pub status: Option<u16>,
    pub request_body: Body,
    pub response_body: Body,
    /// Last time a response byte arrived; with [`RequestEvent::at`] this is the duration.
    pub finished_at: Option<DateTime<Local>>,
    /// Why there is nothing to inspect, for the entries where there is nothing.
    pub note: Option<String>,
}

impl Exchange {
    pub fn record_request_chunk(&mut self, chunk: &[u8]) {
        self.request_body.push(chunk);
    }

    pub fn record_response_chunk(&mut self, chunk: &[u8]) {
        self.response_body.push(chunk);
        self.finished_at = Some(Local::now());
    }

    fn evict_bodies(&mut self) {
        self.request_body.bytes = Vec::new();
        self.request_body.evicted = true;
        self.response_body.bytes = Vec::new();
        self.response_body.evicted = true;
    }

    /// Whether anything beyond the request line was ever visible.
    pub fn is_opaque(&self) -> bool {
        self.request_headers.is_empty() && self.response_headers.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct RequestEvent {
    pub id: u64,
    pub at: DateTime<Local>,
    pub method: String,
    pub url: String,
    pub kind: EventKind,
    /// Shared with the request handler, which keeps writing into it while the body streams.
    pub exchange: Arc<Mutex<Exchange>>,
}

impl RequestEvent {
    pub fn host(&self) -> &str {
        self.url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.url)
            .split('/')
            .next()
            .unwrap_or(&self.url)
    }

    /// Path and query, or `/` when the URL carries none.
    pub fn path(&self) -> &str {
        let rest = self
            .url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.url);
        match rest.find('/') {
            Some(at) => &rest[at..],
            None => "/",
        }
    }

    /// Registrable-ish domain for grouping: the last two labels, so `a.cdn.example.com` and
    /// `b.cdn.example.com` land together.
    pub fn domain(&self) -> String {
        let host = self.host().split(':').next().unwrap_or_default();
        if host.parse::<std::net::IpAddr>().is_ok() {
            return host.to_owned();
        }
        let labels: Vec<&str> = host.split('.').collect();
        if labels.len() <= 2 {
            return host.to_owned();
        }
        labels[labels.len() - 2..].join(".")
    }

    pub fn note(self, note: impl Into<String>) -> Self {
        if let Ok(mut exchange) = self.exchange.lock() {
            exchange.note = Some(note.into());
        }
        self
    }
}

#[derive(Default)]
pub struct Counters {
    pub proxied: AtomicU64,
    pub blocked: AtomicU64,
    pub tunneled: AtomicU64,
    pub modified: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> CountersSnapshot {
        CountersSnapshot {
            proxied: self.proxied.load(Ordering::Relaxed),
            blocked: self.blocked.load(Ordering::Relaxed),
            tunneled: self.tunneled.load(Ordering::Relaxed),
            modified: self.modified.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CountersSnapshot {
    pub proxied: u64,
    pub blocked: u64,
    pub tunneled: u64,
    pub modified: u64,
}

pub struct ProxyState {
    status: Mutex<Status>,
    filters: Mutex<FiltersStatus>,
    events: Mutex<VecDeque<RequestEvent>>,
    next_id: AtomicU64,
    mode: AtomicU8,
    paused: AtomicBool,
    pub counters: Counters,
}

impl ProxyState {
    pub fn new(mode: MitmMode) -> Self {
        Self {
            status: Mutex::new(Status::Stopped),
            filters: Mutex::new(FiltersStatus::Idle),
            events: Mutex::new(VecDeque::with_capacity(MAX_LOGGED_REQUESTS)),
            paused: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            mode: AtomicU8::new(mode as u8),
            counters: Counters::default(),
        }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or(Status::Stopped)
    }

    pub fn set_status(&self, status: Status) {
        if let Ok(mut guard) = self.status.lock() {
            *guard = status;
        }
    }

    pub fn filters_status(&self) -> FiltersStatus {
        self.filters
            .lock()
            .map(|status| status.clone())
            .unwrap_or(FiltersStatus::Idle)
    }

    pub fn set_filters_status(&self, status: FiltersStatus) {
        if let Ok(mut guard) = self.filters.lock() {
            *guard = status;
        }
    }

    pub fn mode(&self) -> MitmMode {
        match self.mode.load(Ordering::Relaxed) {
            0 => MitmMode::HostnameOnly,
            _ => MitmMode::Full,
        }
    }

    pub fn set_mode(&self, mode: MitmMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    /// Log an exchange and hand back its [`Exchange`], which the handler keeps writing into as
    /// headers arrive and the body streams.
    /// Whether new exchanges are being added to the log. Traffic still flows when paused; it is
    /// only the log that holds still, which is what stops the ring turning over and bodies being
    /// evicted while an exchange is being read.
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn record(&self, mut event: RequestEvent) -> Arc<Mutex<Exchange>> {
        if self.paused() {
            // Still handed back so the request path writes into something, just never logged —
            // and the counters describe the log, so they hold too.
            return event.exchange.clone();
        }

        match &event.kind {
            EventKind::Blocked { .. } => self.counters.blocked.fetch_add(1, Ordering::Relaxed),
            // Both are a CONNECT that opened rather than a request that was forwarded.
            EventKind::Tunneled | EventKind::Intercepted => {
                self.counters.tunneled.fetch_add(1, Ordering::Relaxed)
            }
            EventKind::Proxied => self.counters.proxied.fetch_add(1, Ordering::Relaxed),
        };

        event.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let exchange = event.exchange.clone();

        if let Ok(mut events) = self.events.lock() {
            if events.len() == MAX_LOGGED_REQUESTS {
                events.pop_back();
            }
            events.push_front(event);
            // Exactly one entry crosses the detail boundary per push, so this stays O(1).
            if let Some(aged_out) = events.get(MAX_DETAILED_REQUESTS) {
                if let Ok(mut aged_out) = aged_out.exchange.lock() {
                    aged_out.evict_bodies();
                }
            }
        }

        exchange
    }

    /// The logged exchange with this id, if it has not aged out.
    pub fn event(&self, id: u64) -> Option<RequestEvent> {
        self.events
            .lock()
            .ok()?
            .iter()
            .find(|event| event.id == id)
            .cloned()
    }

    pub fn note_modified_response(&self) {
        self.counters.modified.fetch_add(1, Ordering::Relaxed);
    }

    /// Copies the most recent events, newest first.
    pub fn recent_events(&self, limit: usize) -> Vec<RequestEvent> {
        self.events
            .lock()
            .map(|events| events.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear_events(&self) {
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
        // The dashboard tiles count what the log holds, so leaving them running would describe
        // requests that no longer exist anywhere in the app.
        self.counters.proxied.store(0, Ordering::Relaxed);
        self.counters.blocked.store(0, Ordering::Relaxed);
        self.counters.tunneled.store(0, Ordering::Relaxed);
        self.counters.modified.store(0, Ordering::Relaxed);
    }
}

impl RequestEvent {
    /// `id` is assigned by [`ProxyState::record`]; until then it is 0.
    pub fn now(method: impl Into<String>, url: impl Into<String>, kind: EventKind) -> Self {
        Self {
            id: 0,
            at: Local::now(),
            method: method.into(),
            url: url.into(),
            kind,
            exchange: Arc::new(Mutex::new(Exchange::default())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(url: &str) -> RequestEvent {
        RequestEvent::now("GET", url, EventKind::Proxied)
    }

    #[test]
    fn splits_host_path_and_domain() {
        let deep = event("https://a.cdn.example.com/x/y?z=1");
        assert_eq!(deep.host(), "a.cdn.example.com");
        assert_eq!(deep.path(), "/x/y?z=1");
        assert_eq!(deep.domain(), "example.com");

        assert_eq!(event("https://example.com/").domain(), "example.com");
        assert_eq!(event("https://localhost/").domain(), "localhost");
        assert_eq!(event("http://93.184.216.34:80/a").domain(), "93.184.216.34");
        assert_eq!(event("https://example.com").path(), "/");
    }

    #[test]
    fn bodies_keep_a_prefix_and_count_the_rest() {
        let mut body = Body::default();
        body.push(&vec![b'a'; MAX_BODY_BYTES - 1]);
        body.push(b"bb");
        assert_eq!(body.bytes.len(), MAX_BODY_BYTES);
        assert_eq!(body.seen, MAX_BODY_BYTES as u64 + 1);
        assert!(body.truncated());
    }

    #[test]
    fn eviction_drops_the_prefix_but_keeps_the_size() {
        let state = ProxyState::new(MitmMode::HostnameOnly);
        let first = state.record(event("https://example.com/first"));
        first.lock().unwrap().record_response_chunk(b"hello");

        for index in 0..MAX_DETAILED_REQUESTS {
            state.record(event(&format!("https://example.com/{index}")));
        }

        let body = &first.lock().unwrap().response_body;
        assert!(body.evicted);
        assert!(body.bytes.is_empty());
        assert_eq!(body.seen, 5);
    }

    #[test]
    fn ids_are_assigned_on_record_and_findable() {
        let state = ProxyState::new(MitmMode::HostnameOnly);
        state.record(event("https://example.com/a"));
        state.record(event("https://example.com/b"));
        let newest = state.recent_events(1).remove(0);
        assert_eq!(newest.id, 2);
        assert_eq!(state.event(2).unwrap().url, "https://example.com/b");
        assert!(state.event(99).is_none());
    }
}
