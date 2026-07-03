//! Guest-side SSH client: drives the native `ssh.*` host ops and feeds bytes to the VT
//! emulator. Also translates egui keyboard events into the byte sequences a PTY expects.

use egui_ios_plugin_sdk::abi::{self, net};
use egui_ios_plugin_sdk::{HostHandle, egui};

use crate::vt::Vt;

pub enum Phase {
    Connecting,
    Ready,
    Ended(String),
}

pub struct SshClient {
    id: u64,
    pub phase: Phase,
    pub vt: Vt,
    pub user: String,
    pub host: String,
    grid: (u16, u16),
}

impl SshClient {
    /// Open a session. `rows` is the PTY height (already minus any local chrome).
    pub fn connect(
        host_h: &HostHandle,
        user: &str,
        host: &str,
        port: u16,
        auth: net::SshAuth,
        cols: u16,
        rows: u16,
    ) -> Result<Self, String> {
        let conn = net::SshConnect {
            host: host.to_string(),
            port,
            user: user.to_string(),
            auth,
            cols,
            rows,
            term: "xterm-256color".to_string(),
        };
        let id_bytes = host_h
            .call(net::op::SSH_CONNECT, &abi::encode(&conn))
            .map_err(|e| format!("{e}"))?;
        let id = net::id_from_bytes(&id_bytes).ok_or("host returned a bad session id")?;
        Ok(SshClient {
            id,
            phase: Phase::Connecting,
            vt: Vt::new(cols as usize, rows as usize),
            user: user.to_string(),
            host: host.to_string(),
            grid: (cols, rows),
        })
    }

    pub fn poll(&mut self, host_h: &HostHandle) {
        if matches!(self.phase, Phase::Ended(_)) {
            return;
        }
        let Ok(bytes) = host_h.call(net::op::SSH_POLL, &net::id_to_bytes(self.id)) else {
            self.phase = Phase::Ended("poll failed".into());
            return;
        };
        let Ok(poll) = abi::decode::<net::SshPoll>(&bytes) else {
            self.phase = Phase::Ended("bad poll response".into());
            return;
        };
        if !poll.data.is_empty() {
            self.vt.feed(&poll.data);
        }
        self.phase = match poll.state {
            net::SshState::Connecting => Phase::Connecting,
            net::SshState::Ready => Phase::Ready,
            net::SshState::Closed(m) => {
                Phase::Ended(if m.is_empty() { "connection closed".into() } else { m })
            }
            net::SshState::Error(e) => Phase::Ended(e),
        };
    }

    pub fn resize(&mut self, host_h: &HostHandle, cols: u16, rows: u16) {
        self.vt.resize(cols as usize, rows as usize);
        if (cols, rows) != self.grid {
            self.grid = (cols, rows);
            let msg = net::SshResize { id: self.id, cols, rows };
            let _ = host_h.call(net::op::SSH_RESIZE, &abi::encode(&msg));
        }
    }

    pub fn write(&self, host_h: &HostHandle, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let msg = net::SshWrite { id: self.id, data: bytes.to_vec() };
        let _ = host_h.call(net::op::SSH_WRITE, &abi::encode(&msg));
    }

    pub fn close(&self, host_h: &HostHandle) {
        let _ = host_h.call(net::op::SSH_CLOSE, &net::id_to_bytes(self.id));
    }
}

/// Translate this frame's egui keyboard events into PTY input bytes. Printable characters
/// arrive as `Text`; control and navigation keys map to their escape sequences. Plain letter
/// `Key` events are ignored (the matching `Text` event carries them) — only Ctrl-combos and
/// non-text keys are handled here, mirroring the local editor's split.
pub fn input_bytes(ui: &egui::Ui) -> Vec<u8> {
    let events = ui.input(|i| i.events.clone());
    let mut out = Vec::new();
    for ev in events {
        match ev {
            egui::Event::Text(t) => {
                for c in t.chars() {
                    if !c.is_control() {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                let ctrl = modifiers.ctrl || modifiers.command || modifiers.mac_cmd;
                push_key(&mut out, key, ctrl);
            }
            _ => {}
        }
    }
    out
}

fn push_key(out: &mut Vec<u8>, key: egui::Key, ctrl: bool) {
    use egui::Key;
    // Ctrl + letter → control byte (Ctrl-A = 0x01 … Ctrl-Z = 0x1a).
    if ctrl {
        let name = key.name();
        let b = name.as_bytes();
        if b.len() == 1 && b[0].is_ascii_alphabetic() {
            out.push(b[0].to_ascii_uppercase() - b'A' + 1);
            return;
        }
    }
    match key {
        Key::Enter => out.push(b'\r'),
        Key::Backspace => out.push(0x7f),
        Key::Tab => out.push(b'\t'),
        Key::Escape => out.push(0x1b),
        Key::ArrowUp => out.extend_from_slice(b"\x1b[A"),
        Key::ArrowDown => out.extend_from_slice(b"\x1b[B"),
        Key::ArrowRight => out.extend_from_slice(b"\x1b[C"),
        Key::ArrowLeft => out.extend_from_slice(b"\x1b[D"),
        Key::Home => out.extend_from_slice(b"\x1b[H"),
        Key::End => out.extend_from_slice(b"\x1b[F"),
        Key::Delete => out.extend_from_slice(b"\x1b[3~"),
        Key::PageUp => out.extend_from_slice(b"\x1b[5~"),
        Key::PageDown => out.extend_from_slice(b"\x1b[6~"),
        _ => {}
    }
}
