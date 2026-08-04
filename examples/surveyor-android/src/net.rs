//! Network plumbing: a reconnecting WebSocket reader thread and one-shot
//! HTTP helpers against the sensing server's REST surface. All plain HTTP —
//! the server lives on the LAN.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crate::proto::{self, WsMsg};

/// Spawn the WS reader; returns a stop flag. Reconnects every 2 s until
/// stopped. Each parsed message is sent through `tx` and wakes the UI.
pub fn spawn_ws(server: String, tx: Sender<WsMsg>, ctx: egui::Context) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    std::thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            let url = format!("ws://{server}/ws/sensing");
            match tungstenite::connect(&url) {
                Ok((mut socket, _resp)) => {
                    let _ = tx.send(WsMsg::Other("connected".to_owned()));
                    ctx.request_repaint();
                    loop {
                        if stop_thread.load(Ordering::Relaxed) {
                            let _ = socket.close(None);
                            return;
                        }
                        match socket.read() {
                            Ok(tungstenite::Message::Text(text)) => {
                                if let Some(msg) = proto::parse(&text) {
                                    if tx.send(msg).is_err() {
                                        return;
                                    }
                                    ctx.request_repaint();
                                }
                            }
                            Ok(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)) => {}
                            Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                }
                Err(_) => {
                    let _ = tx.send(WsMsg::Other("connect failed".to_owned()));
                    ctx.request_repaint();
                }
            }
            std::thread::sleep(Duration::from_secs(2));
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
    tx: Sender<HttpOutcome>,
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
