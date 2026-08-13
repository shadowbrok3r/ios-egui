//! Live probe of the app's WebSocket path against a running sensing server:
//! connects exactly like the Android app and runs every message through the
//! same lenient parser the phone uses. Ignored by default (needs a server).
//!
//! ```text
//! SURVEYOR_SERVER=127.0.0.1:8080 SURVEYOR_SECS=20 \
//!   cargo test -p surveyor_android --test ws_probe -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use surveyor_android::proto::{self, WsMsg};

#[test]
#[ignore = "requires a running sensing server"]
fn probe_live_server() {
    let server = std::env::var("SURVEYOR_SERVER").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let secs: u64 = std::env::var("SURVEYOR_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let url = format!("ws://{server}/ws/sensing");
    println!("connecting {url}");
    let (mut socket, _) = tungstenite::connect(&url).expect("connect");
    // Without a read timeout a silent server hangs the probe past its deadline.
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = socket.get_ref() {
        let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    }

    let (mut sensing, mut mmwave, mut other, mut unparsed) = (0u32, 0u32, 0u32, 0u32);
    let mut a121 = 0u32;
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
                Some(WsMsg::A121(a)) => {
                    a121 += 1;
                    if a121 == 1 {
                        println!("first a121_presence: presence={} dist={:.2}m inter={:.1}", a.presence, a.distance_m, a.inter_score);
                    }
                }
                Some(WsMsg::Other(_)) => other += 1,
                None => unparsed += 1,
            },
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("ws read: {e}"),
        }
    }

    println!("\n{secs}s summary: sensing={sensing} mmwave={mmwave} a121={a121} other={other} unparsed={unparsed}");
    if !last_radar.is_empty() {
        println!("last radar frame: {last_radar}");
    }
    assert_eq!(unparsed, 0, "every server message must parse");
    assert!(sensing > 0, "no sensing_update arrived");
}
