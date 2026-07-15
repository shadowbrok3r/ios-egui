//! Serve a simulated WireLab board over TCP with a UDP discovery beacon:
//! `cargo run --example sim_board`. Lets any plugin host on this machine or
//! the LAN discover and drive a board with no hardware attached.

use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::time::Duration;

use wirelab_core::circuit::Circuit;
use wirelab_core::library::Library;
use wirelab_link::Device;
use wirelab_link::sim::SimDevice;
use wirelab_proto::frame::{Decoder, encode};
use wirelab_proto::{HostMsg, MAX_FRAME};

fn main() {
    // WIRELAB_ASSETS overrides; default assumes the sibling-repo layout.
    let assets = match std::env::var("WIRELAB_ASSETS") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../EmbeddedApps/wirelab/assets"),
    };
    let lib = Library::load(&assets.join("boards"), &assets.join("components"))
        .expect("assets load");
    let board = lib.board("esp32-c5-devkitc-1").expect("board").clone();

    let listener = TcpListener::bind("0.0.0.0:4518").expect("bind 4518");
    let port = listener.local_addr().unwrap().port();
    // The primary route's local address is what LAN peers should dial.
    let ip = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("8.8.8.8:80").and_then(|_| s.local_addr()))
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());
    println!("sim board on {ip}:{port}, beaconing to UDP 4519");

    std::thread::spawn(move || {
        let tx = UdpSocket::bind("0.0.0.0:0").expect("beacon socket");
        tx.set_broadcast(true).ok();
        let beacon = format!("WIRELAB1 {ip} {port} Simulated");
        loop {
            let _ = tx.send_to(beacon.as_bytes(), "255.255.255.255:4519");
            let _ = tx.send_to(beacon.as_bytes(), "127.0.0.1:4519");
            std::thread::sleep(Duration::from_secs(1));
        }
    });

    loop {
        let (mut sock, peer) = match listener.accept() {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        println!("client {peer}");
        sock.set_read_timeout(Some(Duration::from_millis(10))).ok();
        sock.set_nodelay(true).ok();
        let mut dev = SimDevice::new(board.clone(), lib.clone(), Circuit::new(&board.id));
        let mut dec: Decoder<HostMsg> = Decoder::new();
        let mut rx = [0u8; 512];
        let mut out = [0u8; MAX_FRAME];
        'session: loop {
            match sock.read(&mut rx) {
                Ok(0) => break 'session,
                Ok(n) => {
                    for &b in &rx[..n] {
                        if let Some(Ok(msg)) = dec.push(b) {
                            dev.send(&msg).ok();
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break 'session,
            }
            for msg in dev.poll() {
                if let Ok(n) = encode(&msg, &mut out)
                    && sock.write_all(&out[..n]).is_err()
                {
                    break 'session;
                }
            }
        }
        println!("client {peer} gone");
    }
}
