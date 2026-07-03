//! Dev-sync client: polls a `cargo egui-ios plugin serve` server on the LAN and streams
//! rebuilt plugins to the app for hot reload. Plain HTTP/1.0 over `std::net` — no TLS,
//! development only.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Ceiling on a single artifact download; a hostile/broken server cannot OOM the device.
const MAX_BODY_BYTES: u64 = 128 << 20;
/// Re-send an unconfirmed update after this many polls (covers a failed install → retry).
const RETRY_POLLS: u32 = 5;

pub struct PluginUpdate {
    pub id: String,
    pub hash: String,
    pub manifest_toml: String,
    pub wasm: Vec<u8>,
}

/// Background poller; create with [`DevSync::start`], drop (or [`DevSync::stop`]) to end.
pub struct DevSync {
    pub addr: String,
    rx: Receiver<PluginUpdate>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    /// id → hash the host has confirmed installed. The poller resends until this matches, so a
    /// failed install is retried instead of being lost to a premature "seen" marker.
    installed: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

impl DevSync {
    /// `addr` is `host:port` of the dev server, e.g. `192.168.1.50:7878`.
    pub fn start(addr: &str) -> DevSync {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(String::from("connecting…")));
        let installed = Arc::new(Mutex::new(std::collections::HashMap::new()));
        {
            let addr = addr.to_owned();
            let stop = Arc::clone(&stop);
            let status = Arc::clone(&status);
            let installed = Arc::clone(&installed);
            std::thread::Builder::new()
                .name("plugin-devsync".into())
                .spawn(move || poll_loop(&addr, &tx, &stop, &status, &installed))
                .ok();
        }
        DevSync {
            addr: addr.to_owned(),
            rx,
            stop,
            status,
            installed,
        }
    }

    pub fn status(&self) -> String {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub(crate) fn try_recv(&self) -> Option<PluginUpdate> {
        self.rx.try_recv().ok()
    }

    /// Record that `id`@`hash` installed successfully, so the poller stops resending it.
    pub(crate) fn mark_installed(&self, id: &str, hash: &str) {
        if let Ok(mut map) = self.installed.lock() {
            map.insert(id.to_owned(), hash.to_owned());
        }
    }
}

impl Drop for DevSync {
    fn drop(&mut self) {
        self.stop();
    }
}

fn poll_loop(
    addr: &str,
    tx: &Sender<PluginUpdate>,
    stop: &AtomicBool,
    status: &Mutex<String>,
    installed: &Mutex<std::collections::HashMap<String, String>>,
) {
    // id → (hash last sent, polls waited for confirmation).
    let mut pending: std::collections::HashMap<String, (String, u32)> = std::collections::HashMap::new();
    let set_status = |s: String| {
        if let Ok(mut g) = status.lock() {
            *g = s;
        }
    };
    while !stop.load(Ordering::Relaxed) {
        match poll_once(addr, tx, installed, &mut pending) {
            Ok(n) => set_status(format!(
                "connected — {} plugin{} tracked{}",
                pending.len().max(installed.lock().map(|m| m.len()).unwrap_or(0)),
                if pending.len() == 1 { "" } else { "s" },
                if n > 0 { " (pushing update)" } else { "" },
            )),
            Err(e) => set_status(format!("error: {e:#}")),
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    set_status("stopped".into());
}

fn poll_once(
    addr: &str,
    tx: &Sender<PluginUpdate>,
    installed: &Mutex<std::collections::HashMap<String, String>>,
    pending: &mut std::collections::HashMap<String, (String, u32)>,
) -> Result<usize> {
    let index = http_get(addr, "/plugins.json")?;
    let index: serde_json::Value = serde_json::from_slice(&index)?;
    let list = index.as_array().ok_or_else(|| anyhow!("plugins.json: expected array"))?;
    let mut sent = 0;
    for entry in list {
        let id = entry["id"].as_str().unwrap_or_default().to_owned();
        let hash = entry["hash"].as_str().unwrap_or_default().to_owned();
        if id.is_empty() {
            continue;
        }
        // Already confirmed installed at this hash?
        if installed.lock().map(|m| m.get(&id) == Some(&hash)).unwrap_or(false) {
            pending.remove(&id);
            continue;
        }
        // Sent this exact hash recently and still awaiting confirmation — hold off resending.
        if let Some((sent_hash, waited)) = pending.get_mut(&id) {
            if *sent_hash == hash && *waited < RETRY_POLLS {
                *waited += 1;
                continue;
            }
        }

        let wasm_path = entry["wasm"].as_str().unwrap_or_default();
        let manifest_path = entry["manifest"].as_str().unwrap_or_default();
        let wasm = http_get(addr, wasm_path)?;
        // Verify the fetched bytes match the hash the index advertised — otherwise the
        // wasm/manifest pair may be mid-rebuild; skip and retry next poll.
        if format!("{:016x}", fnv1a64(&wasm)) != hash {
            continue;
        }
        let manifest_toml = String::from_utf8(http_get(addr, manifest_path)?)?;
        pending.insert(id.clone(), (hash.clone(), 0));
        sent += 1;
        let _ = tx.send(PluginUpdate { id, hash, manifest_toml, wasm });
    }
    Ok(sent)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Minimal HTTP/1.0 GET. Verifies the status line and, when present, the Content-Length;
/// caps the body at [`MAX_BODY_BYTES`] so a hostile server cannot exhaust device memory.
fn http_get(addr: &str, path: &str) -> Result<Vec<u8>> {
    let sock_addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve {addr}"))?;
    let mut stream = TcpStream::connect_timeout(&sock_addr, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n")?;

    let mut response = Vec::new();
    stream.take(MAX_BODY_BYTES + 1).read_to_end(&mut response)?;
    if response.len() as u64 > MAX_BODY_BYTES {
        bail!("GET {path}: response exceeds {MAX_BODY_BYTES} bytes");
    }

    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let head = std::str::from_utf8(&response[..header_end]).unwrap_or_default();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    if !status_line.split(' ').any(|t| t == "200") {
        bail!("GET {path}: {status_line}");
    }
    let content_length = lines
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok());

    let body = response[header_end + 4..].to_vec();
    if let Some(len) = content_length {
        if body.len() != len {
            bail!("GET {path}: truncated body ({} of {len} bytes)", body.len());
        }
    }
    Ok(body)
}
