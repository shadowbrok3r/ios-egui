//! Settings, filter subscriptions and their on-disk cache.
//!
//! Everything lives under the app's private files directory (`Host::documents_dir`) rather than
//! privaxy's `~/.privaxy`: Android has no home directory, and the private directory is the only
//! location writable without a storage permission.

use crate::proxy::ca::CertAuthority;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "config.json";
const FILTERS_DIR: &str = "filters";
const CA_EXPORT_FILE: &str = "privaxy-ca.crt";

/// How much of the connection the proxy takes apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MitmMode {
    /// Never terminate TLS. Blocked hosts are refused at CONNECT, everything else is tunneled
    /// byte for byte. Needs no certificate installed and works for every app on the device,
    /// including ones that pin certificates.
    HostnameOnly,
    /// Terminate TLS with a minted certificate so full URLs and page content can be filtered.
    /// Requires the CA in the device trust store, and only reaches apps that trust user CAs.
    Full,
}

impl MitmMode {
    pub fn label(self) -> &'static str {
        match self {
            MitmMode::HostnameOnly => "Hostname only",
            MitmMode::Full => "Full inspection",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FilterGroup {
    Ads,
    Privacy,
    Malware,
    Social,
    Annoyances,
    Mobile,
    Regional,
}

impl FilterGroup {
    pub const ALL: [FilterGroup; 7] = [
        FilterGroup::Ads,
        FilterGroup::Privacy,
        FilterGroup::Malware,
        FilterGroup::Social,
        FilterGroup::Annoyances,
        FilterGroup::Mobile,
        FilterGroup::Regional,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FilterGroup::Ads => "Ads",
            FilterGroup::Privacy => "Privacy",
            FilterGroup::Malware => "Malware",
            FilterGroup::Social => "Social",
            FilterGroup::Annoyances => "Annoyances",
            FilterGroup::Mobile => "Mobile",
            FilterGroup::Regional => "Regional",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub enabled: bool,
    pub title: String,
    pub group: FilterGroup,
    pub url: String,
}

impl Filter {
    fn new(enabled: bool, title: &str, group: FilterGroup, url: &str) -> Self {
        Self {
            enabled,
            title: title.to_owned(),
            group,
            url: url.to_owned(),
        }
    }

    /// Filename the downloaded list is cached under.
    pub fn cache_file_name(&self) -> String {
        let stem: String = self
            .title
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        format!("{stem}.txt")
    }
}

/// Subscribed directly from upstream rather than through privaxy's filters CDN, so the app has no
/// single point of failure it does not control.
fn default_filters() -> Vec<Filter> {
    use FilterGroup::*;

    vec![
        Filter::new(true, "EasyList", Ads, "https://easylist.to/easylist/easylist.txt"),
        Filter::new(
            true,
            "EasyPrivacy",
            Privacy,
            "https://easylist.to/easylist/easyprivacy.txt",
        ),
        Filter::new(
            true,
            "uBlock filters",
            Ads,
            "https://ublockorigin.github.io/uAssets/filters/filters.txt",
        ),
        Filter::new(
            true,
            "uBlock privacy",
            Privacy,
            "https://ublockorigin.github.io/uAssets/filters/privacy.txt",
        ),
        Filter::new(
            true,
            "uBlock badware risks",
            Malware,
            "https://ublockorigin.github.io/uAssets/filters/badware.txt",
        ),
        // Mobile in-app advertising is largely absent from the desktop lists.
        Filter::new(
            true,
            "AdGuard Mobile Ads",
            Mobile,
            "https://filters.adtidy.org/extension/ublock/filters/11.txt",
        ),
        Filter::new(
            true,
            "AdGuard Tracking Protection",
            Privacy,
            "https://filters.adtidy.org/extension/ublock/filters/3.txt",
        ),
        Filter::new(
            true,
            "Peter Lowe's ad and tracking servers",
            Ads,
            "https://pgl.yoyo.org/adservers/serverlist.php?hostformat=adblockplus&showintro=0&mimetype=plaintext",
        ),
        Filter::new(
            false,
            "EasyList Cookie",
            Annoyances,
            "https://secure.fanboy.co.nz/fanboy-cookiemonster.txt",
        ),
        Filter::new(
            false,
            "Fanboy Annoyances",
            Annoyances,
            "https://secure.fanboy.co.nz/fanboy-annoyance.txt",
        ),
        Filter::new(
            false,
            "Fanboy Social",
            Social,
            "https://secure.fanboy.co.nz/fanboy-social.txt",
        ),
        Filter::new(
            false,
            "URLhaus malicious URLs",
            Malware,
            "https://malware-filter.gitlab.io/malware-filter/urlhaus-filter-online.txt",
        ),
        // Off by default and listed rather than fetched: EasyList's regional supplements only pay
        // for themselves in the matching language, and the group card is hidden while empty.
        Filter::new(
            false,
            "EasyList Germany",
            Regional,
            "https://easylist.to/easylistgermany/easylistgermany.txt",
        ),
        Filter::new(
            false,
            "EasyList China",
            Regional,
            "https://easylist-downloads.adblockplus.org/easylistchina.txt",
        ),
        Filter::new(
            false,
            "EasyList Italy",
            Regional,
            "https://easylist-downloads.adblockplus.org/easylistitaly.txt",
        ),
        Filter::new(
            false,
            "EasyList Spanish",
            Regional,
            "https://easylist-downloads.adblockplus.org/easylistspanish.txt",
        ),
        Filter::new(
            false,
            "EasyList Dutch",
            Regional,
            "https://easylist-downloads.adblockplus.org/easylistdutch.txt",
        ),
        Filter::new(
            false,
            "Liste FR",
            Regional,
            "https://easylist-downloads.adblockplus.org/liste_fr.txt",
        ),
        Filter::new(
            false,
            "RU AdList",
            Regional,
            "https://easylist-downloads.adblockplus.org/advblock.txt",
        ),
        Filter::new(
            false,
            "ABPindo",
            Regional,
            "https://easylist-downloads.adblockplus.org/abpindo.txt",
        ),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub listen_port: u16,
    /// Bind 0.0.0.0 instead of 127.0.0.1, letting other devices on the network use the phone as
    /// their proxy. Off by default — an open proxy on a shared network is a liability.
    pub share_on_network: bool,
    pub mode: MitmMode,
    pub blocking_enabled: bool,
    pub start_on_launch: bool,
    pub exclusions: BTreeSet<String>,

    /// Hosts terminated even in hostname-only mode — the inverse of [`Self::exclusions`].
    ///
    /// Full inspection device-wide breaks every app that does not trust a user CA, so the usable
    /// shape is to leave the mode alone and name the few hosts worth looking inside.
    #[serde(default)]
    pub intercepts: BTreeSet<String>,
    pub custom_filters: Vec<String>,
    pub filters: Vec<Filter>,
    pub ca: CertAuthority,

    // Capture settings. All defaulted: a configuration written before these existed has to keep
    // parsing, because the fallback is regenerating it — and that would mint a new CA and quietly
    // invalidate the certificate the user installed in Settings.
    /// Claim the tun through `VpnService` and route every app's traffic into the proxy.
    #[serde(default)]
    pub capture_all: bool,
    /// Run a foreground notification so Android stops reclaiming the process when backgrounded.
    #[serde(default = "yes")]
    pub foreground_service: bool,
    /// Route IPv6 into the tun as well. Off, IPv6 traffic bypasses the proxy entirely.
    #[serde(default = "yes")]
    pub capture_ipv6: bool,
    /// Drop UDP 443 so QUIC apps fall back to TCP, which the proxy can see.
    #[serde(default = "yes")]
    pub block_quic: bool,
    /// Resolvers handed to captured apps, comma separated.
    #[serde(default = "default_vpn_dns")]
    pub vpn_dns: String,

    // Persisted UI state. Kept here rather than in a second file so there is one thing to write
    // and one thing to back up; both default, so an older configuration still parses.
    /// The request log's filter, sort and grouping.
    #[serde(default)]
    pub request_filters: crate::ui::requests::RequestFilters,
    /// Wrap headers and bodies in the inspector, rather than scrolling them sideways.
    #[serde(default = "yes")]
    pub inspect_wrap: bool,
}

fn yes() -> bool {
    true
}

fn default_vpn_dns() -> String {
    String::from("1.1.1.1,1.0.0.1")
}

impl Config {
    fn new(ca: CertAuthority) -> Self {
        Self {
            listen_port: 8100,
            share_on_network: false,
            // Works on an unmodified device with nothing installed, so it is what a first run gets.
            mode: MitmMode::HostnameOnly,
            blocking_enabled: true,
            start_on_launch: true,
            exclusions: BTreeSet::new(),
            intercepts: BTreeSet::new(),
            custom_filters: Vec::new(),
            filters: default_filters(),
            ca,
            // Capture needs the system VPN consent dialog, so it is never on for a first run.
            capture_all: false,
            foreground_service: true,
            capture_ipv6: true,
            block_quic: true,
            vpn_dns: default_vpn_dns(),
            request_filters: crate::ui::requests::RequestFilters::default(),
            inspect_wrap: true,
        }
    }

    pub fn enabled_filters(&self) -> impl Iterator<Item = &Filter> {
        self.filters.iter().filter(|filter| filter.enabled)
    }

    /// Resolver addresses for the tun, ignoring blank entries in the stored list.
    pub fn dns_servers(&self) -> Vec<String> {
        let servers: Vec<String> = self
            .vpn_dns
            .split(',')
            .map(str::trim)
            .filter(|server| !server.is_empty())
            .map(str::to_owned)
            .collect();
        if servers.is_empty() {
            vec![String::from("1.1.1.1")]
        } else {
            servers
        }
    }
}

/// Resolved locations under the app's private storage.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub filters_dir: PathBuf,
    pub ca_export: PathBuf,
}

impl Paths {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        Self {
            config_file: root.join(CONFIG_FILE),
            filters_dir: root.join(FILTERS_DIR),
            ca_export: root.join(CA_EXPORT_FILE),
            root,
        }
    }

    /// Loads the stored configuration, generating a CA and defaults on first run. A config that
    /// fails to parse is replaced rather than fatal — a phone has no way to hand-edit it.
    pub fn load_or_create(&self) -> Result<Config, ConfigError> {
        std::fs::create_dir_all(&self.filters_dir)?;

        if let Ok(bytes) = std::fs::read(&self.config_file) {
            match serde_json::from_slice::<Config>(&bytes) {
                Ok(config) => return Ok(config),
                Err(error) => log::warn!("Discarding unreadable configuration: {error}"),
            }
        }

        let config = Config::new(CertAuthority::generate()?);
        self.save(&config)?;
        self.export_ca(&config)?;
        Ok(config)
    }

    pub fn save(&self, config: &Config) -> Result<(), ConfigError> {
        let serialized = serde_json::to_vec_pretty(config)?;
        std::fs::write(&self.config_file, serialized)?;
        Ok(())
    }

    /// Writes the CA certificate on its own so it can be handed to Android as a file.
    pub fn export_ca(&self, config: &Config) -> Result<PathBuf, ConfigError> {
        std::fs::write(&self.ca_export, config.ca.certificate_pem.as_bytes())?;
        Ok(self.ca_export.clone())
    }

    /// Writes the request log as HAR 1.2. Timestamped, so successive captures coexist rather than
    /// overwriting each other.
    pub fn export_capture(
        &self,
        events: &[crate::proxy::state::RequestEvent],
        at: chrono::DateTime<chrono::Local>,
    ) -> Result<PathBuf, ConfigError> {
        let path = self
            .root
            .join(format!("privaxy-{}.har", at.format("%Y%m%d-%H%M%S")));
        let har = crate::proxy::har::build(events);
        std::fs::write(&path, serde_json::to_vec(&har)?)?;
        Ok(path)
    }

    pub fn cached_filter(&self, filter: &Filter) -> Option<String> {
        std::fs::read_to_string(self.filters_dir.join(filter.cache_file_name())).ok()
    }

    pub fn cache_filter(&self, filter: &Filter, contents: &str) -> Result<(), ConfigError> {
        std::fs::write(self.filters_dir.join(filter.cache_file_name()), contents)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("configuration is not valid JSON: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("could not generate a certificate authority: {0}")]
    Rcgen(#[from] rcgen::Error),
}
