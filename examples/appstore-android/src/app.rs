//! The store UI. Android-only: it drives PackageInstaller through `HostExt`.

use crate::icons;

use egui_mobile::egui;
use egui_mobile::{CreateContext, EguiApp, Host, HostExt, app};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const GREEN: egui::Color32 = egui::Color32::from_rgb(90, 220, 120);
const AMBER: egui::Color32 = egui::Color32::from_rgb(240, 200, 90);
const RED: egui::Color32 = egui::Color32::from_rgb(255, 100, 90);

// ---- wire types (appstore /api/apps) ----

#[derive(Clone, Debug, Default, Deserialize)]
struct ApkMeta {
    #[serde(default)]
    version_code: u64,
    #[serde(default)]
    version_name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    published_at: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AppEntry {
    slug: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    package: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    latest: Option<ApkMeta>,
    #[serde(default)]
    has_icon: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Release {
    #[serde(default)]
    version_name: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    published_at: i64,
}

#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
struct Config {
    server_url: String,
    api_key: String,
}

// ---- HTTP ----

/// Trim, drop a trailing slash, and default to http:// when no scheme is given.
fn normalize_url(raw: &str) -> String {
    let s = raw.trim().trim_end_matches('/');
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

/// rustls + bundled webpki roots (ring provider): https with no Android trust-store JNI.
#[cfg(feature = "tls")]
fn client_builder() -> reqwest::ClientBuilder {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder().use_preconfigured_tls(config)
}

#[cfg(not(feature = "tls"))]
fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
}

/// API key as both header spellings plus a JSON Accept, so an expired credential
/// surfaces as a 401 instead of a login-page redirect.
fn build_client(api_key: &str) -> Result<reqwest::Client, String> {
    use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json, */*"));
    let key = api_key.trim();
    if !key.is_empty() {
        if let Ok(mut v) = HeaderValue::from_str(key) {
            v.set_sensitive(true);
            headers.insert("x-api-key", v);
        }
        if let Ok(mut v) = HeaderValue::from_str(&format!("Bearer {key}")) {
            v.set_sensitive(true);
            headers.insert(AUTHORIZATION, v);
        }
    }
    client_builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

/// Icon bytes per app, fetched once alongside the catalog.
async fn fetch_icons(client: &reqwest::Client, base: &str, apps: &[AppEntry]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for app in apps.iter().filter(|a| a.has_icon) {
        // Version the URL so a publish busts any cached copy — including a cached
        // 404 from before the icon existed.
        let v = app.latest.as_ref().map(|m| m.published_at).unwrap_or(0);
        let Ok(resp) = client.get(format!("{base}/{}/icon.png?v={v}", app.slug)).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        if let Ok(bytes) = resp.bytes().await {
            out.push((app.slug.clone(), bytes.to_vec()));
        }
    }
    out
}

async fn fetch_changelog(
    client: &reqwest::Client,
    base: &str,
    slug: &str,
) -> Result<Vec<Release>, String> {
    let resp = client
        .get(format!("{base}/api/apps/{slug}/changelog"))
        .send()
        .await
        .map_err(|e| format!("request failed: {}", describe(&e)))?;
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("bad response: {}", describe(&e)))?;
    serde_json::from_value(body["releases"].clone()).map_err(|e| e.to_string())
}

async fn fetch_apps(client: &reqwest::Client, base: &str) -> Result<Vec<AppEntry>, String> {
    let resp = client
        .get(format!("{base}/api/apps"))
        .send()
        .await
        .map_err(|e| format!("request failed: {}", describe(&e)))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("bad response: {}", describe(&e)))?;
    if !status.is_success() {
        let err = body["error"].as_str().unwrap_or("(no detail)");
        return Err(format!("HTTP {}: {err}", status.as_u16()));
    }
    serde_json::from_value(body["apps"].clone()).map_err(|e| e.to_string())
}

/// Stream the APK to `<dest>.part`, verify sha256 + size, then rename into place.
async fn download_apk(
    client: &reqwest::Client,
    base: &str,
    slug: &str,
    dest: &Path,
    expect_sha256: &str,
    got: Arc<AtomicU64>,
) -> Result<(), String> {
    let mut resp = client
        .get(format!("{base}/{slug}/app.apk"))
        .send()
        .await
        .map_err(|e| format!("request failed: {}", describe(&e)))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let part = dest.with_extension("apk.part");
    let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("download interrupted: {}", describe(&e)))? {
        hasher.update(&chunk);
        std::io::Write::write_all(&mut file, &chunk).map_err(|e| e.to_string())?;
        got.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    drop(file);
    let sum = hex_lower(&hasher.finalize());
    if !expect_sha256.is_empty() && sum != expect_sha256 {
        let _ = std::fs::remove_file(&part);
        return Err("the download didn't match its checksum — try again".into());
    }
    std::fs::rename(&part, dest).map_err(|e| e.to_string())?;
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// reqwest's top-level message is only "error sending request for url (…)"; the
/// actual failure — DNS, TLS, refused connection — is in the source chain, so
/// report the whole chain or a device log says nothing useful.
fn describe(err: &reqwest::Error) -> String {
    let mut out = err.to_string();
    let mut src: Option<&dyn std::error::Error> = std::error::Error::source(err);
    while let Some(e) = src {
        out.push_str(&format!(" <- {e}"));
        src = e.source();
    }
    if err.is_connect() {
        out.push_str(" [connect]");
    }
    if err.is_timeout() {
        out.push_str(" [timeout]");
    }
    out
}

// ---- background jobs ----

struct Job<T> {
    rx: Option<Receiver<T>>,
}

impl<T: Send + 'static> Job<T> {
    fn idle() -> Self {
        Self { rx: None }
    }
    fn busy(&self) -> bool {
        self.rx.is_some()
    }
    fn start(&mut self, ctx: egui::Context, f: impl FnOnce() -> T + Send + 'static) -> bool {
        if self.rx.is_some() {
            return false;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
        true
    }
    fn poll(&mut self) -> Option<T> {
        match self.rx.as_ref()?.try_recv() {
            Ok(v) => {
                self.rx = None;
                Some(v)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                None
            }
        }
    }
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
        .map(|rt| rt.block_on(f))
}

// ---- the app ----

/// Unix seconds to `Y-m-d` (days-from-epoch civil conversion).
fn fmt_date(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn fmt_size(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else {
        format!("{} KB", n >> 10)
    }
}

type Catalog = (Vec<AppEntry>, Vec<(String, Vec<u8>)>);

struct StoreApp {
    cfg: Config,
    cfg_loaded: bool,
    settings_open: bool,
    apps: Vec<AppEntry>,
    icons: std::collections::HashMap<String, egui::TextureHandle>,
    pending_icons: Vec<(String, Vec<u8>)>,
    changelogs: std::collections::HashMap<String, Vec<Release>>,
    changelog_job: Job<Result<(String, Vec<Release>), String>>,
    expanded: Option<String>,
    installed: std::collections::HashMap<String, i64>,
    refresh: Job<Result<Catalog, String>>,
    download: Job<Result<(String, PathBuf), String>>,
    downloading: Option<String>,
    got: Arc<AtomicU64>,
    total: u64,
    // After an install is handed to PackageInstaller, re-read versionCodes for a while.
    watch_until: Option<Instant>,
    last_scan: Instant,
    status: String,
    status_err: bool,
}

impl StoreApp {
    fn new(_cc: &CreateContext) -> Self {
        StoreApp {
            cfg: Config::default(),
            cfg_loaded: false,
            settings_open: false,
            apps: vec![],
            icons: Default::default(),
            pending_icons: vec![],
            changelogs: Default::default(),
            changelog_job: Job::idle(),
            expanded: None,
            installed: Default::default(),
            refresh: Job::idle(),
            download: Job::idle(),
            downloading: None,
            got: Arc::new(AtomicU64::new(0)),
            total: 0,
            watch_until: None,
            last_scan: Instant::now(),
            status: String::new(),
            status_err: false,
        }
    }

    fn config_path(host: &Host) -> Option<PathBuf> {
        let dir = host.documents_dir()?;
        Some(PathBuf::from(dir).join("appstore").join("config.json"))
    }

    fn load_config(&mut self, host: &Host) {
        self.cfg_loaded = true;
        if let Some(path) = Self::config_path(host)
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(cfg) = serde_json::from_str::<Config>(&text)
        {
            self.cfg = cfg;
        }
        if self.cfg.server_url.is_empty() {
            self.settings_open = true;
        }
    }

    fn save_config(&mut self, host: &Host) {
        let Some(path) = Self::config_path(host) else { return };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.cfg) {
            if let Err(e) = std::fs::write(&path, text) {
                self.set_status(format!("config not saved: {e}"), true);
            }
        }
    }

    fn set_status(&mut self, s: impl Into<String>, err: bool) {
        self.status = s.into();
        self.status_err = err;
        log::info!("appstore: {}", self.status);
    }

    fn do_refresh(&mut self, ctx: egui::Context) {
        if self.cfg.server_url.trim().is_empty() {
            self.set_status("set the server URL in settings", true);
            return;
        }
        let base = normalize_url(&self.cfg.server_url);
        let key = self.cfg.api_key.clone();
        let started = self.refresh.start(ctx, move || {
            let client = build_client(&key)?;
            block_on(async move {
                let apps = fetch_apps(&client, &base).await?;
                let icons = fetch_icons(&client, &base, &apps).await;
                Ok((apps, icons))
            })?
        });
        if started {
            self.set_status("refreshing…", false);
        }
    }

    /// Re-read installed versionCodes for everything in the catalog.
    /// Apps whose published build is newer than what is installed.
    fn pending_updates(&self) -> Vec<&AppEntry> {
        self.apps
            .iter()
            .filter(|a| {
                let Some(latest) = a.latest.as_ref() else { return false };
                self.installed
                    .get(&a.package)
                    .is_some_and(|installed| *installed < latest.version_code as i64)
            })
            .collect()
    }

    fn scan_installed(&mut self, host: &Host) {
        self.installed.clear();
        for app in &self.apps {
            if app.package.is_empty() {
                continue;
            }
            if let Some(code) = host.installed_version_code(&app.package) {
                self.installed.insert(app.package.clone(), code);
            }
        }
        self.last_scan = Instant::now();
    }

    fn start_download(&mut self, ctx: egui::Context, host: &Host, entry: &AppEntry) {
        let Some(meta) = entry.latest.clone() else { return };
        let Some(docs) = host.documents_dir() else {
            self.set_status("no app storage available", true);
            return;
        };
        let dir = PathBuf::from(docs).join("appstore");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.set_status(format!("storage: {e}"), true);
            return;
        }
        let dest = dir.join(format!("{}.apk", entry.slug));
        let base = normalize_url(&self.cfg.server_url);
        let key = self.cfg.api_key.clone();
        let slug = entry.slug.clone();
        self.got.store(0, Ordering::Relaxed);
        self.total = meta.size;
        let got = self.got.clone();
        let started = self.download.start(ctx, move || {
            let client = build_client(&key)?;
            block_on(download_apk(&client, &base, &slug, &dest, &meta.sha256, got))??;
            Ok((slug, dest))
        });
        if started {
            self.downloading = Some(entry.slug.clone());
            self.set_status(format!("downloading {}…", entry.name), false);
        } else {
            self.set_status("another download is still running", true);
        }
    }

    fn poll_jobs(&mut self, ctx: &egui::Context, host: &Host) {
        if let Some(result) = self.refresh.poll() {
            match result {
                Ok((apps, icons)) => {
                    let n = apps.len();
                    self.apps = apps;
                    self.pending_icons = icons;
                    self.scan_installed(host);
                    let pending = self.pending_updates().len();
                    self.set_status(
                        match pending {
                            0 => format!("{n} apps · everything is up to date"),
                            1 => format!("{n} apps · 1 update available"),
                            k => format!("{n} apps · {k} updates available"),
                        },
                        false,
                    );
                }
                Err(e) => self.set_status(e, true),
            }
        }
        // Textures must be uploaded on the UI thread, so decode here rather than
        // in the fetch job.
        for (slug, bytes) in std::mem::take(&mut self.pending_icons) {
            if let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png) {
                let rgba = img.into_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                let tex = ctx.load_texture(format!("icon:{slug}"), color, egui::TextureOptions::LINEAR);
                self.icons.insert(slug, tex);
            }
        }
        if let Some(result) = self.changelog_job.poll() {
            match result {
                Ok((slug, rels)) => {
                    self.changelogs.insert(slug, rels);
                }
                Err(e) => self.set_status(format!("changelog: {e}"), true),
            }
        }
        if let Some(result) = self.download.poll() {
            self.downloading = None;
            match result {
                Ok((slug, path)) => {
                    self.set_status("handing to the installer…", false);
                    host.self_update(path.to_string_lossy());
                    self.watch_until = Some(Instant::now() + Duration::from_secs(60));
                    let _ = slug;
                }
                Err(e) => self.set_status(e, true),
            }
        }
        // While an install dialog may be in flight, poll versionCodes so the row flips
        // to "up to date" once the system finishes — and read the installer's own verdict, so a
        // refusal says why instead of looking like the download simply did nothing.
        if self.watch_until.is_some() {
            match host.take_install_status() {
                0 => {}
                1 => {
                    self.watch_until = None;
                    self.scan_installed(host);
                    self.set_status("installed", false);
                }
                _ => {
                    self.watch_until = None;
                    let why = host.install_message();
                    self.set_status(
                        if why.is_empty() {
                            "the installer refused it".to_string()
                        } else {
                            format!("install failed — {why}")
                        },
                        true,
                    );
                }
            }
        }
        if let Some(until) = self.watch_until {
            if Instant::now() > until {
                self.watch_until = None;
            } else if self.last_scan.elapsed() > Duration::from_secs(2) {
                self.scan_installed(host);
                ctx.request_repaint_after(Duration::from_secs(2));
            }
        }
        if self.refresh.busy() || self.download.busy() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.label("server");
        ui.add(
            egui::TextEdit::singleline(&mut self.cfg.server_url)
                .hint_text("https://apps.kingsofalchemy.com")
                .desired_width(f32::INFINITY),
        );
        ui.label("API key");
        ui.add(
            egui::TextEdit::singleline(&mut self.cfg.api_key)
                .password(true)
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);
        if ui.button("Save & connect").clicked() {
            self.save_config(host);
            self.settings_open = false;
            self.do_refresh(ui.ctx().clone());
        }
    }

    /// A letter tile for apps whose APK carried no launcher icon.
    fn show_icon(&self, ui: &mut egui::Ui, entry: &AppEntry, side: f32) {
        match self.icons.get(&entry.slug) {
            Some(tex) => {
                ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(side, side)).corner_radius(side * 0.22));
            }
            None => {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                ui.painter().rect_filled(rect, side * 0.22, egui::Color32::from_rgb(116, 109, 187));
                let letter = entry.name.chars().next().unwrap_or('?').to_uppercase().to_string();
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    letter,
                    egui::FontId::proportional(side * 0.45),
                    egui::Color32::WHITE,
                );
            }
        }
    }

    /// One card per row, spanning the full width — the phone layout.
    fn show_app_card(&mut self, ui: &mut egui::Ui, host: &Host, entry: &AppEntry, own_package: &str) {
        let installed = self.installed.get(&entry.package).copied();
        let latest_code = entry.latest.as_ref().map(|m| m.version_code as i64);
        let ctx = ui.ctx().clone();
        egui::Frame::group(ui.style())
            .corner_radius(10.0)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    self.show_icon(ui, entry, 64.0);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(&entry.name).strong().size(18.0));
                            if entry.package == own_package {
                                ui.label(egui::RichText::new("(this app)").small().color(AMBER));
                            }
                        });
                        if !entry.package.is_empty() {
                            ui.label(
                                egui::RichText::new(&entry.package)
                                    .monospace()
                                    .small()
                                    .color(egui::Color32::from_gray(130)),
                            );
                        }
                        let state = match (installed, latest_code) {
                            (Some(i), Some(l)) if i < l => ("update available", AMBER),
                            (Some(_), Some(_)) => ("up to date", GREEN),
                            (Some(_), None) => ("installed", GREEN),
                            (None, _) if entry.package.is_empty() => ("no package name set", RED),
                            (None, _) => ("not installed", egui::Color32::from_gray(150)),
                        };
                        ui.label(egui::RichText::new(format!("{} {}", icons::DOT, state.0)).small().color(state.1));
                    });
                });

                match &entry.latest {
                    Some(m) => {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "v{} · {} · {}",
                                m.version_name,
                                fmt_size(m.size),
                                fmt_date(m.published_at)
                            ))
                            .small(),
                        );
                        if !m.notes.is_empty() {
                            ui.label(egui::RichText::new(&m.notes).color(egui::Color32::from_gray(170)));
                        }
                    }
                    None => {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new("nothing published").weak());
                    }
                }

                if !entry.notes.is_empty() {
                    ui.label(egui::RichText::new(&entry.notes).small().color(egui::Color32::from_gray(150)));
                }

                ui.add_space(10.0);
                let this_downloading = self.downloading.as_deref() == Some(entry.slug.as_str());
                if this_downloading {
                    let got = self.got.load(Ordering::Relaxed);
                    let frac = if self.total > 0 { got as f32 / self.total as f32 } else { 0.0 };
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(ui.available_width())
                            .text(format!("{} / {}", fmt_size(got), fmt_size(self.total))),
                    );
                } else if entry.latest.is_some() {
                    // Two columns: install left, changelog right. Splitting the row means a
                    // thumb aiming at one has no chance of landing on the other.
                    let open = self.expanded.as_deref() == Some(entry.slug.as_str());
                    let gap = ui.spacing().item_spacing.x;
                    let half = ((ui.available_width() - gap) / 2.0).max(80.0);
                    ui.horizontal(|ui| {
                        let label = match (installed, latest_code) {
                            (Some(i), Some(l)) if i < l => "Update",
                            (None, Some(_)) => "Install",
                            _ => "Reinstall",
                        };
                        let can = host.can_install_packages() && !self.download.busy();
                        let install = egui::Button::new(
                            egui::RichText::new(format!("{} {label}", icons::INSTALL)).size(16.0),
                        )
                        .min_size(egui::vec2(half, 46.0));
                        if ui.add_enabled(can, install).clicked() {
                            self.start_download(ctx.clone(), host, entry);
                        }

                        let toggle = egui::Button::new(
                            egui::RichText::new(format!("{} Changelog", icons::CHANGELOG)).size(16.0),
                        )
                        .min_size(egui::vec2(half, 46.0))
                        .selected(open);
                        if ui.add(toggle).clicked() {
                            self.expanded = if open { None } else { Some(entry.slug.clone()) };
                            if !open && !self.changelogs.contains_key(&entry.slug) {
                                self.fetch_changelog(ctx.clone(), entry.slug.clone());
                            }
                        }
                    });

                    if open {
                        ui.add_space(4.0);
                        match self.changelogs.get(&entry.slug) {
                            Some(rels) if rels.is_empty() => {
                                ui.label(egui::RichText::new("no entries yet").weak().small());
                            }
                            Some(rels) => {
                                for r in rels {
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "v{}  {}",
                                            r.version_name,
                                            fmt_date(r.published_at)
                                        ))
                                        .strong()
                                        .small(),
                                    );
                                    if !r.notes.is_empty() {
                                        ui.label(
                                            egui::RichText::new(&r.notes)
                                                .small()
                                                .color(egui::Color32::from_gray(165)),
                                        );
                                    }
                                }
                            }
                            None => {
                                ui.spinner();
                            }
                        }
                    }
                }
            });
    }

    fn fetch_changelog(&mut self, ctx: egui::Context, slug: String) {
        let base = normalize_url(&self.cfg.server_url);
        let key = self.cfg.api_key.clone();
        self.changelog_job.start(ctx, move || {
            let client = build_client(&key)?;
            let rels = block_on(fetch_changelog(&client, &base, &slug))??;
            Ok((slug, rels))
        });
    }
}

impl EguiApp for StoreApp {
    fn theme(&self, ctx: &egui::Context) {
        let mut v = egui::Visuals::dark();
        v.panel_fill = egui::Color32::BLACK;
        v.window_fill = egui::Color32::from_rgb(12, 12, 12);
        ctx.set_visuals(v);
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        if !self.cfg_loaded {
            self.load_config(host);
            if !self.cfg.server_url.is_empty() {
                self.do_refresh(ui.ctx().clone());
            }
        }
        let ctx = ui.ctx().clone();
        self.poll_jobs(&ctx, host);

        ui.horizontal(|ui| {
            ui.heading("App Store");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(format!("{} Settings", icons::SETTINGS)).clicked() {
                    self.settings_open = !self.settings_open;
                }
                if self.refresh.busy() {
                    ui.spinner();
                }
            });
        });
        // One button re-reads the catalog and every installed versionCode, so
        // "is anything new?" is a single tap rather than one check per app.
        let check = egui::Button::new(
            egui::RichText::new(format!("{}  Check for updates", icons::REFRESH)).size(16.0),
        )
        .min_size(egui::vec2(ui.available_width(), 46.0));
        if ui.add_enabled(!self.refresh.busy(), check).clicked() {
            self.do_refresh(ctx.clone());
        }
        if !self.status.is_empty() {
            let color = if self.status_err { RED } else { egui::Color32::from_gray(150) };
            ui.label(egui::RichText::new(&self.status).small().color(color));
        }
        ui.separator();

        if self.settings_open {
            self.show_settings(ui, host);
            ui.separator();
        }

        if !host.can_install_packages() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} Installing needs the \"install unknown apps\" grant.",
                        icons::WARN
                    ))
                    .color(AMBER),
                );
                if ui.button("Grant").clicked() {
                    host.request_install_permission();
                }
            });
            ui.separator();
        }

        let own_package = "com.kingsofalchemy.appstore";
        let entries = self.apps.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            if entries.is_empty() && !self.refresh.busy() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("no apps yet — connect in Settings").weak());
                });
            }
            for entry in &entries {
                self.show_app_card(ui, host, entry, own_package);
                ui.add_space(6.0);
            }
        });
    }
}

app!(StoreApp::new);
