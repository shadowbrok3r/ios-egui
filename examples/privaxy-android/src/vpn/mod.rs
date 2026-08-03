//! Transparent capture: the `VpnService` lifecycle, and the tun-to-proxy relay it feeds.
//!
//! A Wi-Fi proxy is set per network, misses mobile data entirely, and is ignored by any app that
//! does not read the setting. A `VpnService` is the only way Android lets an app see the rest, so
//! this claims the tun, turns the packets on it back into connections, and points those at the
//! proxy already listening on loopback.
//!
//! The proxy's own upstream connections must not come back through the tun, or every proxied
//! request would be captured and fed into itself. `VpnService.Builder.addDisallowedApplication`
//! excludes this whole process, which covers reqwest, the filter downloads and the DNS forwarder
//! without any of them needing to know about the VPN.
//!
//! Blocking policy stays in the proxy. Nothing here consults the filter engine: a captured flow
//! becomes a `CONNECT`, and the proxy answers it exactly as it answers the browser's, so there is
//! one place where a host is blocked and one request log that records it.

pub mod dns;
pub mod sniff;

#[cfg(unix)]
pub mod relay;
#[cfg(unix)]
mod tun;

use dns::DnsMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long an address keeps the name it was resolved for. Long enough to outlive a page's worth
/// of connections, short enough that a recycled CDN address is not mislabelled for the rest of
/// the session.
const DNS_TTL: Duration = Duration::from_secs(10 * 60);
const DNS_CAPACITY: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VpnStatus {
    Off,
    /// The system consent dialog is open.
    Requesting,
    Starting,
    Running,
    Failed(String),
}

impl VpnStatus {
    pub fn is_on(&self) -> bool {
        matches!(self, VpnStatus::Requesting | VpnStatus::Starting | VpnStatus::Running)
    }
}

#[derive(Default)]
pub struct VpnStats {
    pub tcp_flows: AtomicU64,
    pub udp_flows: AtomicU64,
    pub dns_queries: AtomicU64,
    pub quic_dropped: AtomicU64,
    /// Flows the proxy refused (a blocked host) or that never reached it.
    pub refused: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VpnStatsSnapshot {
    pub tcp_flows: u64,
    pub udp_flows: u64,
    pub dns_queries: u64,
    pub quic_dropped: u64,
    pub refused: u64,
}

impl VpnStats {
    pub fn snapshot(&self) -> VpnStatsSnapshot {
        VpnStatsSnapshot {
            tcp_flows: self.tcp_flows.load(Ordering::Relaxed),
            udp_flows: self.udp_flows.load(Ordering::Relaxed),
            dns_queries: self.dns_queries.load(Ordering::Relaxed),
            quic_dropped: self.quic_dropped.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
        }
    }
}

/// What the tun is built with, resolved from the stored configuration at start.
#[derive(Debug, Clone)]
pub struct CaptureSettings {
    pub proxy_port: u16,
    pub mtu: u16,
    pub capture_ipv6: bool,
    pub block_quic: bool,
    pub dns: Vec<String>,
}

fn always_on_message(other: &str) -> String {
    format!(
        "{other} is set as Android's always-on VPN, so it holds the tun and Android will not even \
         offer Privaxy the choice. Turn always-on off for it under VPN settings, then try again."
    )
}

pub struct VpnController {
    status: VpnStatus,
    stats: Arc<VpnStats>,
    dns: Arc<DnsMap>,
    /// The relay's own runtime, so a proxy restart does not take the tun down with it.
    runtime: Option<tokio::runtime::Runtime>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Why the relay stopped on its own, left here for the next [`VpnController::poll`].
    relay_error: Arc<Mutex<Option<String>>>,
    /// Held between asking for consent and the descriptor arriving.
    pending: Option<CaptureSettings>,
    /// Foreground notification is up (with or without a tun).
    foreground: bool,
    /// Notification body to fall back to when capture stops but the proxy keeps running.
    foreground_detail: String,
}

impl Default for VpnController {
    fn default() -> Self {
        Self::new()
    }
}

impl VpnController {
    pub fn new() -> Self {
        Self {
            status: VpnStatus::Off,
            stats: Arc::new(VpnStats::default()),
            dns: Arc::new(DnsMap::new(DNS_CAPACITY, DNS_TTL)),
            runtime: None,
            shutdown: None,
            relay_error: Arc::new(Mutex::new(None)),
            pending: None,
            foreground: false,
            foreground_detail: String::from("Proxy running"),
        }
    }

    pub fn status(&self) -> &VpnStatus {
        &self.status
    }

    pub fn stats(&self) -> VpnStatsSnapshot {
        self.stats.snapshot()
    }

    /// Addresses currently attributable to a hostname from DNS answers.
    pub fn known_hosts(&self) -> usize {
        self.dns.len()
    }

    /// Whether the platform can capture at all — false everywhere but Android.
    pub fn is_supported(&self) -> bool {
        platform::SUPPORTED
    }

    /// Android's VPN settings: where this app shows up once consent is granted, and the only place
    /// it can be revoked or made always-on.
    pub fn open_vpn_settings(&self) {
        platform::open_vpn_settings();
    }

    /// This app's notification settings, without which the ongoing service row and its Stop button
    /// are never drawn.
    pub fn open_notification_settings(&self) {
        platform::open_notification_settings();
    }

    /// Run the foreground notification so Android stops reclaiming the process while the proxy is
    /// listening. Idempotent.
    pub fn keep_alive(&mut self, detail: &str) {
        self.foreground_detail = detail.to_owned();
        if self.foreground {
            return;
        }
        self.foreground = platform::start_foreground("Privaxy", detail);
    }

    /// Ask for VPN consent if it is not already granted, then capture.
    pub fn start_capture(&mut self, settings: CaptureSettings) {
        if self.status.is_on() {
            return;
        }
        if let Some(other) = platform::blocking_always_on_vpn() {
            self.status = VpnStatus::Failed(always_on_message(&other));
            return;
        }
        self.pending = Some(settings);
        if platform::request_consent() {
            self.begin();
        } else {
            self.status = VpnStatus::Requesting;
        }
    }

    /// Close the tun, leaving the foreground notification up for the proxy.
    pub fn stop_capture(&mut self, detail: &str) {
        self.foreground_detail = detail.to_owned();
        self.pending = None;
        self.teardown();
        self.release_service();
        self.status = VpnStatus::Off;
    }

    /// Stop capturing and drop the foreground notification with it.
    pub fn shutdown(&mut self) {
        self.pending = None;
        self.teardown();
        platform::stop();
        self.foreground = false;
        self.status = VpnStatus::Off;
    }

    /// Drain the consent result and the service callbacks. Returns true when the status changed,
    /// so the caller knows to save or repaint.
    pub fn poll(&mut self) -> bool {
        let before = self.status.clone();

        if self.status == VpnStatus::Requesting {
            match platform::poll_consent() {
                Some(true) => self.begin(),
                Some(false) => {
                    self.pending = None;
                    // A dialog that never appeared reports the same cancellation as one the user
                    // said no to, so the always-on holder is what tells the two apart.
                    self.status = VpnStatus::Failed(match platform::blocking_always_on_vpn() {
                        Some(other) => always_on_message(&other),
                        None => String::from("Android's VPN permission was declined."),
                    });
                }
                None => {}
            }
        }

        while let Some(event) = platform::poll_event() {
            match event {
                platform::Event::Started { fd, mtu } => self.attach(fd, mtu),
                // Running is the only state where a stop is news: elsewhere it is this side's own
                // teardown echoing back, and acting on it would cancel a start already in flight
                // or overwrite the reason capture failed.
                platform::Event::Stopped if self.status == VpnStatus::Running => {
                    // Revoked in Settings, or another VPN took the slot. Both stop the whole
                    // service, so the foreground notification went with it.
                    self.teardown();
                    self.status = VpnStatus::Off;
                    self.foreground = false;
                }
                platform::Event::Stopped => {}
                platform::Event::Failed(reason) => self.fail(reason),
            }
        }

        // The relay stopping on its own leaves the service running with a dead tun; the service
        // has to come down with it or the next start would find one already established. Taken
        // either way, so a failure cannot outlive the session that produced it.
        let relay_error = self.relay_error.lock().ok().and_then(|mut slot| slot.take());
        if let Some(reason) = relay_error {
            if self.status.is_on() {
                self.fail(reason);
            }
        }

        before != self.status
    }

    /// Put the service back to whatever the proxy still needs, and record why capture stopped.
    fn fail(&mut self, reason: String) {
        self.pending = None;
        self.teardown();
        self.release_service();
        self.status = VpnStatus::Failed(reason);
    }

    fn release_service(&mut self) {
        if self.foreground {
            platform::stop_capture("Privaxy", &self.foreground_detail);
        } else {
            platform::stop();
        }
    }

    /// Consent is granted: start the service, which establishes the tun and calls back.
    fn begin(&mut self) {
        let Some(settings) = self.pending.clone() else {
            self.status = VpnStatus::Off;
            return;
        };

        let config = platform::Config {
            session: String::from("Privaxy"),
            mtu: settings.mtu,
            address: String::from("10.7.0.1"),
            prefix: 32,
            address6: settings.capture_ipv6.then(|| String::from("fd00:7::1")),
            prefix6: 128,
            dns: settings.dns.clone(),
            notification_title: String::from("Privaxy"),
            notification_text: format!("Capturing all traffic on port {}", settings.proxy_port),
        };

        if platform::start(&config) {
            self.foreground = true;
            self.status = VpnStatus::Starting;
        } else {
            self.pending = None;
            self.status = VpnStatus::Failed(String::from("Could not start the VPN service."));
        }
    }

    /// The descriptor arrived; run the relay over it.
    #[cfg(unix)]
    fn attach(&mut self, fd: std::os::fd::RawFd, mtu: u16) {
        use std::os::fd::FromRawFd;

        let Some(settings) = self.pending.take() else {
            // Nothing asked for this; close the descriptor rather than leak the interface.
            drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
            return;
        };

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("privaxy-vpn")
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
                self.status = VpnStatus::Failed(format!("Could not start the VPN runtime: {error}"));
                return;
            }
        };

        // Java detached the descriptor, so the relay owns it: dropping it closes the interface.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let config = relay::RelayConfig {
            proxy: (std::net::Ipv4Addr::LOCALHOST, settings.proxy_port).into(),
            mtu,
            block_quic: settings.block_quic,
        };

        let stats = self.stats.clone();
        let dns = self.dns.clone();
        let failure = self.relay_error.clone();
        runtime.spawn(async move {
            if let Some(reason) = relay::run(owned, config, stats, dns, shutdown_rx).await {
                if let Ok(mut slot) = failure.lock() {
                    *slot = Some(reason);
                }
            }
        });

        self.runtime = Some(runtime);
        self.shutdown = Some(shutdown_tx);
        self.status = VpnStatus::Running;
    }

    #[cfg(not(unix))]
    fn attach(&mut self, _fd: i32, _mtu: u16) {
        self.status = VpnStatus::Failed(String::from("Capture is only supported on Android."));
    }

    fn teardown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(runtime) = self.runtime.take() {
            // Waiting for in-flight flows would stall the frame loop; the relay closes the
            // descriptor as it unwinds.
            runtime.shutdown_background();
        }
    }
}

impl Drop for VpnController {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// The `VpnService` surface, or a stub off Android so the app still builds and tests on the host.
#[cfg(target_os = "android")]
mod platform {
    pub use egui_mobile::vpn::{
        VpnConfig as Config, VpnEvent as Event, blocking_always_on_vpn,
        open_notification_settings, open_vpn_settings, poll_consent, poll_event, request_consent,
        start, start_foreground, stop, stop_capture,
    };

    pub const SUPPORTED: bool = true;
}

// Never constructed off Android; it exists so the app and its tests still build on the host.
#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
mod platform {
    pub const SUPPORTED: bool = false;

    #[derive(Debug, Clone)]
    pub struct Config {
        pub session: String,
        pub mtu: u16,
        pub address: String,
        pub prefix: u8,
        pub address6: Option<String>,
        pub prefix6: u8,
        pub dns: Vec<String>,
        pub notification_title: String,
        pub notification_text: String,
    }

    #[derive(Debug, Clone)]
    pub enum Event {
        Started { fd: i32, mtu: u16 },
        Stopped,
        Failed(String),
    }

    pub fn request_consent() -> bool {
        false
    }
    pub fn poll_consent() -> Option<bool> {
        Some(false)
    }
    pub fn poll_event() -> Option<Event> {
        None
    }
    pub fn start(_config: &Config) -> bool {
        false
    }
    pub fn start_foreground(_title: &str, _text: &str) -> bool {
        false
    }
    pub fn stop_capture(_title: &str, _text: &str) -> bool {
        false
    }
    pub fn stop() {}
    pub fn open_vpn_settings() -> bool {
        false
    }
    pub fn open_notification_settings() -> bool {
        false
    }
    pub fn blocking_always_on_vpn() -> Option<String> {
        None
    }
}
