//! Network plumbing: a reconnecting WebSocket reader thread and one-shot
//! HTTP helpers against the sensing server's REST surface. All plain HTTP —
//! the server lives on the LAN.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::Duration;

use crate::proto::{self, WsMsg};

/// Connection state and data, kept distinct so a server message whose `type`
/// the parser doesn't recognize can never masquerade as a status change.
#[derive(Debug, Clone)]
pub enum WsEvent {
    Connected,
    Disconnected(String),
    Msg(WsMsg),
}

/// How long a blocking read waits before the loop re-checks the stop flag.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Idle ticks before a keepalive ping goes out (~10 s).
const PING_AFTER_IDLE_TICKS: u32 = 5;
/// Idle ticks with no traffic at all before the link is declared dead (~30 s).
const DEAD_AFTER_IDLE_TICKS: u32 = 15;

fn dial(server: &str) -> Result<TcpStream, String> {
    let addr = server
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "no address".to_string())?;
    // connect_timeout, unlike tungstenite's plain connect, bounds SYN retries
    // so a wrong host doesn't pin the thread for minutes.
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(4))
        .map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}

/// Spawn the WS reader; returns a stop flag. Reconnects every 2 s until
/// stopped. Each event is sent through `tx` and wakes the UI.
pub fn spawn_ws(server: String, tx: SyncSender<WsEvent>, ctx: egui::Context) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    std::thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            let url = format!("ws://{server}/ws/sensing");
            let attempt = dial(&server).and_then(|stream| {
                tungstenite::client(url.as_str(), stream).map_err(|e| e.to_string())
            });
            match attempt {
                Ok((mut socket, _resp)) => {
                    if tx.try_send(WsEvent::Connected).is_err() {
                        // Receiver gone: this reader belongs to a replaced session.
                        return;
                    }
                    ctx.request_repaint();
                    let mut idle_ticks = 0u32;
                    let reason = loop {
                        if stop_thread.load(Ordering::Relaxed) {
                            let _ = socket.close(None);
                            return;
                        }
                        match socket.read() {
                            Ok(tungstenite::Message::Text(text)) => {
                                idle_ticks = 0;
                                if let Some(msg) = proto::parse(&text) {
                                    // Bounded queue: when the UI stops rendering
                                    // (screen off), drop rather than grow forever.
                                    let _ = tx.try_send(WsEvent::Msg(msg));
                                    ctx.request_repaint();
                                }
                            }
                            Ok(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)) => {
                                idle_ticks = 0;
                            }
                            Ok(tungstenite::Message::Close(_)) => break "closed by server".to_owned(),
                            Ok(_) => idle_ticks = 0,
                            Err(tungstenite::Error::Io(e))
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                // Read timeout: a liveness tick, not a failure.
                                idle_ticks += 1;
                                if idle_ticks >= DEAD_AFTER_IDLE_TICKS {
                                    break "no data".to_owned();
                                }
                                if idle_ticks % PING_AFTER_IDLE_TICKS == 0
                                    && socket.send(tungstenite::Message::Ping(Vec::new().into())).is_err()
                                {
                                    break "ping failed".to_owned();
                                }
                            }
                            Err(e) => break e.to_string(),
                        }
                    };
                    let _ = tx.try_send(WsEvent::Disconnected(reason));
                    ctx.request_repaint();
                }
                Err(e) => {
                    let _ = tx.try_send(WsEvent::Disconnected(e));
                    ctx.request_repaint();
                }
            }
            // Sleep in slices so a stop lands promptly.
            for _ in 0..4 {
                if stop_thread.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
    stop
}

/// Result of a one-shot HTTP call, delivered back to the UI thread.
#[derive(Debug, Clone)]
pub struct HttpOutcome {
    pub label: String,
    pub result: Result<serde_json::Value, String>,
}

fn post(server: &str, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    let url = format!("http://{server}{path}");
    let mut req = minreq::post(&url).with_timeout(4);
    if let Some(b) = body {
        req = req
            .with_header("Content-Type", "application/json")
            .with_body(b.to_string());
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    serde_json::from_str(resp.as_str().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

fn get(server: &str, path: &str) -> Result<serde_json::Value, String> {
    let url = format!("http://{server}{path}");
    let resp = minreq::get(&url).with_timeout(4).send().map_err(|e| e.to_string())?;
    serde_json::from_str(resp.as_str().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// Fire an HTTP call on a worker thread; the outcome lands on `tx`.
pub fn spawn_http(
    server: String,
    label: String,
    call: HttpCall,
    tx: std::sync::mpsc::Sender<HttpOutcome>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let result = match call {
            HttpCall::LocStatus => get(&server, "/api/v1/localization/status"),
            HttpCall::RecordStart { session } => post(
                &server,
                "/api/v1/localization/record/start",
                Some(serde_json::json!({ "session": session })),
            ),
            HttpCall::RecordStop => post(&server, "/api/v1/localization/record/stop", None),
            HttpCall::Baseline => post(&server, "/api/v1/localization/calibrate/baseline", None),
        };
        let _ = tx.send(HttpOutcome { label, result });
        ctx.request_repaint();
    });
}

#[derive(Debug, Clone)]
pub enum HttpCall {
    LocStatus,
    RecordStart { session: String },
    RecordStop,
    Baseline,
}
