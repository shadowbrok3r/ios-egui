//! End-to-end: the plugin's link logic driving the REAL host TCP/UDP ops
//! (`egui_ios_plugin_host::NetOps`) against a REAL simulated WireLab board
//! (`wirelab_link::sim::SimDevice`) over a loopback socket. This exercises the
//! whole production socket path — everything except the wasm guest boundary,
//! which is generic host infrastructure shared with the other net plugins.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_ios_plugin_host::NetOps;
use wirelab_core::circuit::Circuit;
use wirelab_core::library::Library;
use wirelab_link::Device;
use wirelab_link::sim::SimDevice;
use wirelab_proto::frame::{Decoder, encode};
use wirelab_proto::{HostMsg, MAX_FRAME};

use wirelab_panel::link::{BoardLink, LinkState, Ops, Scanner};

/// `NetOps` as the plugin's op surface, matching the host-call contract
/// (`None` = op not owned by this backend).
struct NetOpsShim(NetOps);

impl Ops for NetOpsShim {
    fn call(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        match self.0.handle(op, payload) {
            Some(r) => r,
            None => Err(format!("unhandled op {op}")),
        }
    }
}

fn assets_dir() -> std::path::PathBuf {
    std::path::Path::new("/home/shadowbroker/Desktop/wirelab/assets").to_path_buf()
}

/// Serve one SimDevice over TCP on 127.0.0.1, mirroring the firmware/board_server
/// loop. Returns the bound address and a stop flag.
fn spawn_board() -> (String, Arc<AtomicBool>) {
    let assets = assets_dir();
    let lib = Library::load(&assets.join("boards"), &assets.join("components"))
        .expect("assets load");
    let board = lib.board("esp32-c5-devkitc-1").expect("board").clone();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);

    std::thread::spawn(move || {
        listener.set_nonblocking(true).ok();
        while !stop_thread.load(Ordering::Relaxed) {
            let (mut sock, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            sock.set_read_timeout(Some(Duration::from_millis(10))).ok();
            sock.set_nodelay(true).ok();
            let mut dev = SimDevice::new(board.clone(), lib.clone(), Circuit::new(&board.id));
            let mut dec: Decoder<HostMsg> = Decoder::new();
            let mut rx = [0u8; 512];
            let mut out = [0u8; MAX_FRAME];
            while !stop_thread.load(Ordering::Relaxed) {
                match sock.read(&mut rx) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &b in &rx[..n] {
                            if let Some(Ok(msg)) = dec.push(b) {
                                dev.send(&msg).ok();
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
                for msg in dev.poll() {
                    if let Ok(n) = encode(&msg, &mut out)
                        && sock.write_all(&out[..n]).is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    (addr, stop)
}

/// Pump the link until `pred` holds or the deadline passes.
fn drive_until(
    link: &mut BoardLink,
    ops: &dyn Ops,
    label: &str,
    pred: impl Fn(&BoardLink) -> bool,
) {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(6);
    while Instant::now() < deadline {
        let now = start.elapsed().as_secs_f64();
        link.poll(ops, now);
        if pred(link) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timeout waiting for {label}: state={:?}", link.state);
}

#[test]
fn plugin_link_drives_a_simulated_board_over_real_sockets() {
    let (addr, stop) = spawn_board();
    let ops = NetOpsShim(NetOps::new());
    let mut link = BoardLink::default();

    link.connect(&ops, &addr);
    assert_eq!(link.state, LinkState::Connecting);

    // Handshake completes over the real TCP op path. The SimDevice board
    // identifies itself as "Simulated"; a real C5 would report "ESP32-C5".
    drive_until(&mut link, &ops, "Ready", |l| l.state == LinkState::Ready);
    let info = link.info.expect("board info");
    assert_eq!(info.chip.name(), "Simulated");
    assert!(info.gpio_mask != 0, "board advertised its usable GPIOs");

    // The board streams telemetry (link auto-requested it on HelloAck).
    drive_until(&mut link, &ops, "telemetry", |l| l.uptime_ms > 0 || l.levels != 0 || !l.log.is_empty());

    // Commands reach the board: drive GPIO2 high and see it reflected.
    link.send(&ops, &HostMsg::SetPinMode { pin: 2, mode: wirelab_proto::PinMode::Output });
    link.send(&ops, &HostMsg::WriteDigital { pin: 2, high: true });
    drive_until(&mut link, &ops, "GPIO2 high", |l| l.levels & (1 << 2) != 0);

    link.disconnect(&ops);
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn scanner_discovers_a_beacon_over_real_udp() {
    use std::net::UdpSocket;

    let ops = NetOpsShim(NetOps::new());
    let mut scanner = Scanner::default();
    // First poll binds the UDP listener on 4519.
    scanner.poll(&ops, 0.0);
    if let Some(err) = &scanner.error {
        // Port 4519 may be busy on a dev box; skip rather than flake.
        eprintln!("skipping: discovery bind failed ({err})");
        return;
    }

    let tx = UdpSocket::bind("127.0.0.1:0").expect("tx bind");
    tx.set_broadcast(true).ok();
    let beacon = b"WIRELAB1 127.0.0.1 4518 Simulated C5";

    let start = Instant::now();
    let mut found = false;
    while start.elapsed() < Duration::from_secs(3) && !found {
        let _ = tx.send_to(beacon, "127.0.0.1:4519");
        std::thread::sleep(Duration::from_millis(100));
        scanner.poll(&ops, start.elapsed().as_secs_f64());
        // The bind error lands asynchronously (the listener thread races the
        // first poll); a dev box running WireLab holds 4519 legitimately.
        if let Some(err) = &scanner.error {
            eprintln!("skipping: discovery bind failed ({err})");
            scanner.close(&ops);
            return;
        }
        found = scanner.boards().any(|b| b.addr == "127.0.0.1:4518" && b.chip == "Simulated C5");
    }
    scanner.close(&ops);
    assert!(found, "beacon was not discovered over real UDP");
}
