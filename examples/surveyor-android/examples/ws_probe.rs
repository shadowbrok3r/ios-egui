//! Host-side probe of the app's WebSocket path against a live sensing server:
//! connects exactly like the Android app and runs every message through the
//! same lenient parser the phone uses.
//!
//! `cargo run -p surveyor_android --example ws_probe -- 127.0.0.1:8080 20`

use std::time::{Duration, Instant};

use surveyor_android::proto::{self, WsMsg};

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "127.0.0.1:8080".into());
    let secs: u64 = args
        .next()
        .map(|s| s.parse().map_err(|_| "bad seconds".to_string()))
        .transpose()?
        .unwrap_or(20);

    let url = format!("ws://{server}/ws/sensing");
    println!("connecting {url}");
    let (mut socket, _) = tungstenite::connect(&url).map_err(|e| e.to_string())?;

    let (mut sensing, mut mmwave, mut other, mut unparsed) = (0u32, 0u32, 0u32, 0u32);
    let mut last_radar = String::new();
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => match proto::parse(&text) {
                Some(WsMsg::Sensing(s)) => {
                    sensing += 1;
                    if sensing == 1 {
                        println!("first sensing_update: source={} nodes={}", s.source, s.node_count);
                    }
                }
                Some(WsMsg::Mmwave(m)) => {
                    mmwave += 1;
                    last_radar = m
                        .targets
                        .iter()
                        .map(|t| format!("({:.2},{:.2}) {:+.2}m/s", t.x_m, t.y_m, t.speed_mps))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if mmwave == 1 {
                        println!("first mmwave_targets: node {} [{last_radar}]", m.node_id);
                    }
                }
                Some(WsMsg::Other(_)) => other += 1,
                None => unparsed += 1,
            },
            Ok(_) => {}
            Err(e) => return Err(format!("ws read: {e}")),
        }
    }

    println!("\n{secs}s summary: sensing={sensing} mmwave={mmwave} other={other} unparsed={unparsed}");
    if !last_radar.is_empty() {
        println!("last radar frame: {last_radar}");
    }
    if mmwave == 0 {
        return Err("no mmwave_targets arrived — shim or server arm not working".into());
    }
    Ok(())
}
