//! The proxy: a Tokio runtime on its own threads, driven from the egui frame loop through
//! [`ProxyHandle`] and observed through [`ProxyState`].

pub mod blocker;
pub mod ca;
pub mod cert;
pub mod config;
pub mod exclusions;
pub mod har;
pub mod session;
pub mod state;

use blocker::FilterEngine;
use cert::CertCache;
use config::{Config, Filter, MitmMode, Paths};
use exclusions::ExclusionStore;
use session::Session;
use state::{FiltersStatus, ProxyState, Status};

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// Phones have far less headroom than the desktop build's 1024.
const MAX_CONNECTIONS: usize = 256;
const FILTER_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60 * 12);

/// The subscription set the engine is built from. Shared with the periodic refresher so a rule
/// added after start is not wiped by the next twelve-hourly rebuild.
type Subscriptions = Arc<std::sync::Mutex<(Vec<Filter>, Vec<String>)>>;

pub struct ProxyHandle {
    runtime: Option<tokio::runtime::Runtime>,
    shutdown: Option<oneshot::Sender<()>>,
    pub state: Arc<ProxyState>,
    engine: Arc<FilterEngine>,
    exclusions: ExclusionStore,
    intercepts: ExclusionStore,
    client: reqwest::Client,
    paths: Paths,
    subscriptions: Subscriptions,
}

impl ProxyHandle {
    /// Spawns the runtime and starts listening. Failures land in [`ProxyState::status`] rather than
    /// propagating, so the UI can show them.
    pub fn start(config: &Config, paths: Paths, state: Arc<ProxyState>) -> Self {
        state.set_status(Status::Starting);
        state.set_mode(config.mode);

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("privaxy")
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                state.set_status(Status::Failed(format!("Could not start runtime: {error}")));
                return Self::stopped(state, paths);
            }
        };

        install_crypto_provider();

        let client = build_client();
        let engine = Arc::new(FilterEngine::empty());
        engine.set_enabled(config.blocking_enabled);
        let exclusions = ExclusionStore::new(config.exclusions.iter().cloned().collect());
        let intercepts = ExclusionStore::new(config.intercepts.iter().cloned().collect());

        let certs = match CertCache::new(&config.ca) {
            Ok(certs) => certs,
            Err(error) => {
                state.set_status(Status::Failed(format!(
                    "Certificate authority unusable: {error}"
                )));
                runtime.shutdown_background();
                return Self::stopped(state, paths);
            }
        };

        let session = Arc::new(Session {
            engine: engine.clone(),
            client: client.clone(),
            certs,
            exclusions: exclusions.clone(),
            intercepts: intercepts.clone(),
            state: state.clone(),
            tls_client_config: Arc::new(client_tls_config()),
        });

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let bind_address = SocketAddr::new(
            if config.share_on_network {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            } else {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            },
            config.listen_port,
        );
        let share_on_network = config.share_on_network;

        runtime.spawn(serve(session, bind_address, share_on_network, shutdown_rx, state.clone()));

        let subscriptions: Subscriptions = Arc::new(std::sync::Mutex::new((
            config.filters.clone(),
            config.custom_filters.clone(),
        )));

        runtime.spawn(refresh_filters_periodically(
            client.clone(),
            paths.clone(),
            subscriptions.clone(),
            engine.clone(),
            state.clone(),
        ));

        Self {
            runtime: Some(runtime),
            shutdown: Some(shutdown_tx),
            state,
            engine,
            exclusions,
            intercepts,
            client,
            paths,
            subscriptions,
        }
    }

    fn stopped(state: Arc<ProxyState>, paths: Paths) -> Self {
        Self {
            runtime: None,
            shutdown: None,
            state,
            engine: Arc::new(FilterEngine::empty()),
            exclusions: ExclusionStore::default(),
            intercepts: ExclusionStore::default(),
            client: build_client(),
            paths,
            subscriptions: Arc::new(std::sync::Mutex::new((Vec::new(), Vec::new()))),
        }
    }

    pub fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(runtime) = self.runtime.take() {
            // Returns immediately; waiting for in-flight tunnels would stall the frame loop.
            runtime.shutdown_background();
        }
        self.state.set_status(Status::Stopped);
    }

    pub fn set_blocking_enabled(&self, enabled: bool) {
        self.engine.set_enabled(enabled);
    }

    pub fn set_mode(&self, mode: MitmMode) {
        self.state.set_mode(mode);
    }

    pub fn set_exclusions(&self, exclusions: Vec<String>) {
        self.exclusions.replace(exclusions);
    }

    pub fn set_intercepts(&self, intercepts: Vec<String>) {
        self.intercepts.replace(intercepts);
    }

    /// Re-sends a captured request on the proxy's own runtime and logs the result as a new row.
    ///
    /// The filter engine is deliberately not consulted: a replay is an explicit act, and having
    /// it silently blocked by a rule the user just added would be the opposite of a debugging aid.
    pub fn replay(&self, method: String, url: String, headers: Vec<(String, String)>, body: Vec<u8>) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let client = self.client.clone();
        let state = self.state.clone();
        runtime.spawn(async move {
            let Ok(parsed) = reqwest::Method::from_bytes(method.as_bytes()) else {
                return;
            };
            let exchange = state.record(
                state::RequestEvent::now(&method, &url, state::EventKind::Proxied)
                    .note("Replayed from the inspector."),
            );

            let mut request = client.request(parsed, &url);
            for (name, value) in &headers {
                // Hop-by-hop headers describe the old connection; content-length is recomputed.
                let lowered = name.to_ascii_lowercase();
                if session::HOP_BY_HOP_HEADERS.contains(&lowered.as_str()) || lowered == "content-length" {
                    continue;
                }
                request = request.header(name, value);
            }
            if !body.is_empty() {
                request = request.body(body.clone());
            }
            if let Ok(mut open) = exchange.lock() {
                open.request_headers = headers;
                open.record_request_chunk(&body);
            }

            match request.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let response_headers = response
                        .headers()
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.as_str().to_owned(),
                                value.to_str().unwrap_or_default().to_owned(),
                            )
                        })
                        .collect();
                    let bytes = response.bytes().await.unwrap_or_default();
                    if let Ok(mut open) = exchange.lock() {
                        open.status = Some(status);
                        open.response_headers = response_headers;
                        open.record_response_chunk(&bytes);
                        open.finished_at = Some(chrono::Local::now());
                    }
                }
                Err(error) => {
                    if let Ok(mut open) = exchange.lock() {
                        open.note = Some(format!("Replay failed: {error}"));
                        open.finished_at = Some(chrono::Local::now());
                    }
                }
            }
        });
    }

    /// Rebuilds the engine from `filters` + `custom`. With `force` the lists are re-downloaded;
    /// without it the on-disk cache is used and a rule change costs no network at all.
    ///
    /// The new set also becomes what the periodic refresher rebuilds from, so a rule added here
    /// survives the next twelve-hourly refresh.
    pub fn reload_filters(&self, filters: Vec<Filter>, custom: Vec<String>, force: bool) {
        if let Ok(mut subscriptions) = self.subscriptions.lock() {
            *subscriptions = (filters.clone(), custom.clone());
        }

        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        runtime.spawn(load_filters(
            self.client.clone(),
            self.paths.clone(),
            filters,
            custom,
            self.engine.clone(),
            self.state.clone(),
            force,
        ));
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn serve(
    session: Arc<Session>,
    bind_address: SocketAddr,
    share_on_network: bool,
    mut shutdown: oneshot::Receiver<()>,
    state: Arc<ProxyState>,
) {
    let listener = match bind_with_retry(bind_address).await {
        Ok(listener) => listener,
        Err(error) => {
            state.set_status(Status::Failed(format!(
                "Could not listen on {bind_address}: {error}"
            )));
            return;
        }
    };

    let display_host = if share_on_network {
        local_ip().unwrap_or_else(|| bind_address.ip().to_string())
    } else {
        String::from("127.0.0.1")
    };
    state.set_status(Status::Running {
        address: format!("{display_host}:{}", bind_address.port()),
    });
    log::info!("Privaxy listening on {bind_address}");

    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    loop {
        let stream = tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    log::warn!("Accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };

        if connections.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONNECTIONS {
            log::warn!("Connection limit reached, dropping connection");
            continue;
        }
        connections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let session = session.clone();
        let connections = connections.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                session::handle(session.clone(), request)
            });

            let result = hyper::server::conn::http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .with_upgrades()
                .await;

            connections.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

            if let Err(error) = result {
                log::debug!("Connection closed: {error}");
            }
        });
    }

    state.set_status(Status::Stopped);
}

/// Restarting drops the old runtime in the background, so the previous listener can still hold the
/// port for a moment. Retrying turns that race into a short delay instead of a failed restart.
async fn bind_with_retry(address: SocketAddr) -> std::io::Result<TcpListener> {
    let mut last_error = None;
    for attempt in 0..8 {
        match TcpListener::bind(address).await {
            Ok(listener) => return Ok(listener),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("bind failed")))
}

async fn refresh_filters_periodically(
    client: reqwest::Client,
    paths: Paths,
    subscriptions: Subscriptions,
    engine: Arc<FilterEngine>,
    state: Arc<ProxyState>,
) {
    // Read on every pass rather than captured once: a snapshot taken at start would rebuild the
    // engine twelve hours later without any rule the user has added since.
    let current = || {
        subscriptions
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    };

    // First pass prefers the cache so blocking is live within a second of launch.
    let (filters, custom) = current();
    load_filters(
        client.clone(),
        paths.clone(),
        filters,
        custom,
        engine.clone(),
        state.clone(),
        false,
    )
    .await;

    loop {
        tokio::time::sleep(FILTER_REFRESH_INTERVAL).await;
        let (filters, custom) = current();
        load_filters(
            client.clone(),
            paths.clone(),
            filters,
            custom,
            engine.clone(),
            state.clone(),
            true,
        )
        .await;
    }
}

async fn load_filters(
    client: reqwest::Client,
    paths: Paths,
    filters: Vec<Filter>,
    custom: Vec<String>,
    engine: Arc<FilterEngine>,
    state: Arc<ProxyState>,
    force: bool,
) {
    let enabled: Vec<Filter> = filters.into_iter().filter(|filter| filter.enabled).collect();
    let total = enabled.len();
    state.set_filters_status(FiltersStatus::Updating {
        completed: 0,
        total,
    });

    let mut lists = Vec::with_capacity(total + 1);
    let mut failures = Vec::new();

    for (index, filter) in enabled.iter().enumerate() {
        if !force {
            if let Some(cached) = paths.cached_filter(filter) {
                lists.push(cached);
                state.set_filters_status(FiltersStatus::Updating {
                    completed: index + 1,
                    total,
                });
                continue;
            }
        }

        match download(&client, &filter.url).await {
            Ok(contents) => {
                if let Err(error) = paths.cache_filter(filter, &contents) {
                    log::warn!("Could not cache {}: {error}", filter.title);
                }
                lists.push(contents);
            }
            Err(error) => {
                log::warn!("Could not download {}: {error}", filter.title);
                // A stale copy still blocks; only a list never fetched is a real gap.
                match paths.cached_filter(filter) {
                    Some(cached) => lists.push(cached),
                    None => failures.push(filter.title.clone()),
                }
            }
        }

        state.set_filters_status(FiltersStatus::Updating {
            completed: index + 1,
            total,
        });
    }

    if !custom.is_empty() {
        lists.push(custom.join("\n"));
    }

    let loaded = lists.len();
    // Parsing a full list set is seconds of CPU; keep it off the runtime's worker threads.
    let _ = tokio::task::spawn_blocking(move || engine.replace(&lists)).await;

    state.set_filters_status(if failures.is_empty() {
        FiltersStatus::Ready { lists: loaded }
    } else {
        FiltersStatus::Failed(format!("Could not fetch: {}", failures.join(", ")))
    });
}

async fn download(client: &reqwest::Client, url: &str) -> Result<String, reqwest::Error> {
    client
        .get(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await
}

fn install_crypto_provider() {
    // Idempotent: the second call returns Err and is ignored.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn client_tls_config() -> rustls::ClientConfig {
    install_crypto_provider();

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// Uses a bundled Mozilla root set rather than rustls-platform-verifier, which needs Android Java
/// helper classes this APK does not carry and panics when uninitialized.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(client_tls_config())
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(8)
        .build()
        .unwrap_or_default()
}

/// The address other devices reach this phone on. Reading it off a connected UDP socket needs no
/// route table access and sends nothing.
fn local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}
