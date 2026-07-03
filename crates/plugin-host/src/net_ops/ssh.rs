//! Interactive SSH sessions over a PTY, backed by russh (ring crypto).
//!
//! Each session owns a thread with a current-thread tokio runtime. The op thread talks to it
//! through an unbounded command channel (stdin, resize, close) and a shared output buffer the
//! `ssh.poll` op drains. Nothing here blocks the host UI thread.
//!
//! Host-key verification is currently trust-all: intended for reaching your own machines over
//! an already-encrypted overlay (Tailscale/WireGuard). Known-hosts TOFU is a follow-up.

use std::sync::{Arc, Mutex};

use egui_ios_plugin_abi as abi;
use abi::net::{SshAuth, SshConnect, SshPoll, SshResize, SshState, SshWrite};

use russh::client::{self, AuthResult};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, decode_secret_key};
use russh::{ChannelMsg, keys::ssh_key};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Cap on output buffered between polls; oldest bytes drop past this so a firehose
/// (`yes`, `cat bigfile`) with a slow poller can't grow memory without bound.
const MAX_OUT_BUFFER: usize = 1 << 20;

/// Default per-op timeout inside the session (connect/auth) in seconds.
const CONNECT_TIMEOUT_SECS: u64 = 20;

/// Commands the op thread sends to a live session's task.
enum Cmd {
    Write(Vec<u8>),
    Resize(u16, u16),
    Close,
}

struct Shared {
    state: SshState,
    out: Vec<u8>,
}

pub(super) struct Session {
    cmd_tx: UnboundedSender<Cmd>,
    shared: Arc<Mutex<Shared>>,
}

impl super::NetHub {
    pub(super) fn ssh_connect(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let conn: SshConnect = abi::decode(payload).map_err(|e| format!("bad SshConnect: {e}"))?;
        let id = self.new_id();
        let shared = Arc::new(Mutex::new(Shared {
            state: SshState::Connecting,
            out: Vec::new(),
        }));
        let (cmd_tx, cmd_rx) = unbounded_channel::<Cmd>();
        let shared_thread = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ssh-session".into())
            .spawn(move || run_thread(conn, cmd_rx, shared_thread))
            .map_err(|e| format!("spawn ssh thread: {e}"))?;
        super::lock(&self.ssh)?.insert(id, Session { cmd_tx, shared });
        Ok(abi::net::id_to_bytes(id))
    }

    pub(super) fn ssh_poll(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let id = abi::net::id_from_bytes(payload).ok_or("bad session id")?;
        let mut map = super::lock(&self.ssh)?;
        let poll = match map.get(&id) {
            Some(sess) => {
                let (state, data) = {
                    let mut sh = sess
                        .shared
                        .lock()
                        .map_err(|_| "ssh session poisoned".to_string())?;
                    (sh.state.clone(), std::mem::take(&mut sh.out))
                };
                // Once terminal and drained, forget the session so the map can't leak.
                if state.is_terminal() {
                    map.remove(&id);
                }
                SshPoll { state, data }
            }
            None => SshPoll {
                state: SshState::Error("unknown or closed session id".into()),
                data: Vec::new(),
            },
        };
        Ok(abi::encode(&poll))
    }

    pub(super) fn ssh_write(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let w: SshWrite = abi::decode(payload).map_err(|e| format!("bad SshWrite: {e}"))?;
        let map = super::lock(&self.ssh)?;
        let sess = map.get(&w.id).ok_or("unknown session id")?;
        sess.cmd_tx
            .send(Cmd::Write(w.data))
            .map_err(|_| "session closed".to_string())?;
        Ok(Vec::new())
    }

    pub(super) fn ssh_resize(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let r: SshResize = abi::decode(payload).map_err(|e| format!("bad SshResize: {e}"))?;
        let map = super::lock(&self.ssh)?;
        let sess = map.get(&r.id).ok_or("unknown session id")?;
        sess.cmd_tx
            .send(Cmd::Resize(r.cols, r.rows))
            .map_err(|_| "session closed".to_string())?;
        Ok(Vec::new())
    }

    pub(super) fn ssh_close(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let id = abi::net::id_from_bytes(payload).ok_or("bad session id")?;
        let mut map = super::lock(&self.ssh)?;
        if let Some(sess) = map.remove(&id) {
            let _ = sess.cmd_tx.send(Cmd::Close);
        }
        Ok(Vec::new())
    }
}

fn set_state(shared: &Mutex<Shared>, state: SshState) {
    if let Ok(mut sh) = shared.lock() {
        // Never overwrite a terminal state with another one (keep the first cause).
        if !sh.state.is_terminal() {
            sh.state = state;
        }
    }
}

fn push_out(shared: &Mutex<Shared>, data: &[u8]) {
    if let Ok(mut sh) = shared.lock() {
        let total = sh.out.len() + data.len();
        if total > MAX_OUT_BUFFER {
            let drop = (total - MAX_OUT_BUFFER).min(sh.out.len());
            sh.out.drain(0..drop);
        }
        sh.out.extend_from_slice(data);
    }
}

fn run_thread(conn: SshConnect, cmd_rx: UnboundedReceiver<Cmd>, shared: Arc<Mutex<Shared>>) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            set_state(&shared, SshState::Error(format!("tokio runtime: {e}")));
            return;
        }
    };
    let result = rt.block_on(run_session(conn, cmd_rx, &shared));
    match result {
        Ok(()) => set_state(&shared, SshState::Closed(String::new())),
        Err(e) => set_state(&shared, SshState::Error(e)),
    }
}

/// Accepts any host key — see the module note.
struct TrustAllHandler;

impl client::Handler for TrustAllHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn run_session(
    conn: SshConnect,
    mut cmd_rx: UnboundedReceiver<Cmd>,
    shared: &Mutex<Shared>,
) -> Result<(), String> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        ..Default::default()
    });

    let connect = client::connect(config, (conn.host.as_str(), conn.port), TrustAllHandler);
    let mut handle = tokio::time::timeout(
        std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
        connect,
    )
    .await
    .map_err(|_| "connection timed out".to_string())?
    .map_err(|e| format!("connect: {e}"))?;

    let authed = match conn.auth {
        SshAuth::Password(password) => handle
            .authenticate_password(&conn.user, password)
            .await
            .map_err(|e| format!("auth: {e}"))?,
        SshAuth::Key { pem, passphrase } => {
            let key = decode_secret_key(&pem, passphrase.as_deref())
                .map_err(|e| format!("private key: {e}"))?;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), Some(HashAlg::Sha256));
            handle
                .authenticate_publickey(&conn.user, key)
                .await
                .map_err(|e| format!("auth: {e}"))?
        }
    };
    if !matches!(authed, AuthResult::Success) {
        return Err("authentication failed".into());
    }

    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("open channel: {e}"))?;
    channel
        .request_pty(
            false,
            &conn.term,
            conn.cols as u32,
            conn.rows as u32,
            0,
            0,
            &[],
        )
        .await
        .map_err(|e| format!("request pty: {e}"))?;
    channel
        .request_shell(false)
        .await
        .map_err(|e| format!("request shell: {e}"))?;

    set_state(shared, SshState::Ready);

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Write(bytes)) => {
                    let _ = channel.data(bytes.as_slice()).await;
                }
                Some(Cmd::Resize(cols, rows)) => {
                    let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                }
                Some(Cmd::Close) | None => {
                    let _ = channel.eof().await;
                    break;
                }
            },
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) => push_out(shared, &data[..]),
                Some(ChannelMsg::ExtendedData { data, .. }) => push_out(shared, &data[..]),
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    push_out(shared, format!("\r\n[exit status {exit_status}]\r\n").as_bytes());
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            },
        }
    }
    Ok(())
}
