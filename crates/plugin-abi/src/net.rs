//! Payload types for the built-in network host ops (`net.http.*`, `ssh.*`).
//!
//! These are op payloads — opaque bytes by convention, postcard-encoded — not part of
//! [`ABI_VERSION`](crate::ABI_VERSION) or [`WIRE_FORMAT`](crate::WIRE_FORMAT). Adding a field
//! or variant here does not break already-installed plugins; only the host and the plugins
//! that call these ops need to agree.
//!
//! All network ops are non-blocking: a `*.request`/`connect` op returns a `u64` handle (as
//! [`id_to_bytes`]), and the plugin polls for progress. This keeps the UI thread responsive
//! while the host runs I/O on background threads.

use serde::{Deserialize, Serialize};

/// Canonical op-name strings, shared so host and guest can't drift on a typo.
pub mod op {
    pub const HTTP_REQUEST: &str = "net.http.request";
    pub const HTTP_POLL: &str = "net.http.poll";
    pub const HTTP_CANCEL: &str = "net.http.cancel";

    pub const SSH_CONNECT: &str = "ssh.connect";
    pub const SSH_POLL: &str = "ssh.poll";
    pub const SSH_WRITE: &str = "ssh.write";
    pub const SSH_RESIZE: &str = "ssh.resize";
    pub const SSH_CLOSE: &str = "ssh.close";
}

/// A handle returned by a `*.request`/`connect` op, little-endian.
pub fn id_to_bytes(id: u64) -> Vec<u8> {
    id.to_le_bytes().to_vec()
}

/// Decode a handle from an op payload; `None` if the byte length is wrong.
pub fn id_from_bytes(bytes: &[u8]) -> Option<u64> {
    bytes.try_into().ok().map(u64::from_le_bytes)
}

// ── HTTP ────────────────────────────────────────────────────────────────────

/// Payload for [`op::HTTP_REQUEST`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Overall timeout; 0 means the host default.
    pub timeout_ms: u32,
}

impl Default for HttpRequest {
    fn default() -> Self {
        HttpRequest {
            method: "GET".into(),
            url: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout_ms: 0,
        }
    }
}

/// A completed HTTP response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Result of [`op::HTTP_POLL`]. The host drops the slot after returning a terminal state, so
/// poll again only while `Pending`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HttpPoll {
    Pending,
    Done(HttpResponse),
    Error(String),
}

// ── SSH ───────────────────────────────────────────────────────────────────────

/// How to authenticate an SSH session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SshAuth {
    Password(String),
    /// PEM-encoded private key (OpenSSH or PKCS#8) plus an optional passphrase.
    Key {
        pem: String,
        passphrase: Option<String>,
    },
}

/// Payload for [`op::SSH_CONNECT`]. Opens a shell on a PTY of the given size.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshConnect {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    pub cols: u16,
    pub rows: u16,
    /// `TERM` value, e.g. `"xterm-256color"`.
    pub term: String,
}

/// Session lifecycle state reported by [`op::SSH_POLL`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshState {
    Connecting,
    Ready,
    /// Closed cleanly (carries an exit note or empty).
    Closed(String),
    /// Failed to connect/authenticate or dropped with an error.
    Error(String),
}

impl SshState {
    /// Whether the session has reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, SshState::Closed(_) | SshState::Error(_))
    }
}

/// Result of [`op::SSH_POLL`]: the current state plus any output bytes produced since the
/// previous poll (drained, so each byte is delivered once).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshPoll {
    pub state: SshState,
    pub data: Vec<u8>,
}

/// Payload for [`op::SSH_WRITE`]: stdin bytes for a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshWrite {
    pub id: u64,
    pub data: Vec<u8>,
}

/// Payload for [`op::SSH_RESIZE`]: a new PTY window size.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshResize {
    pub id: u64,
    pub cols: u16,
    pub rows: u16,
}

/// Topic a plugin emits (and the terminal listens for) to hand off an SSH target — e.g. the
/// Devices plugin asking the terminal to connect to a tailnet host. Payload: [`SshOpenRequest`].
pub const EVENT_SSH_OPEN: &str = "ssh.open";

/// Cross-plugin request to open an SSH session to a host. Carries no credentials; the
/// receiving terminal prompts for the password or key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshOpenRequest {
    pub host: String,
    pub user: String,
    pub port: u16,
}
