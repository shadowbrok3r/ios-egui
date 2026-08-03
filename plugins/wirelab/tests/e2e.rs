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
use wirelab_panel::runner::LiveRunner;
use wirelab_panel::view::BoardModel;

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
    // WIRELAB_ASSETS overrides; default assumes the sibling-repo layout.
    match std::env::var("WIRELAB_ASSETS") {
        Ok(p) => p.into(),
        Err(_) => std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../EmbeddedApps/wirelab/assets"),
    }
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

/// The on-device live runner against a real simulated board: auto-wire an
/// LED, run its script and a rules program, and watch the GPIO go high over
/// the production socket path.
#[test]
fn runner_drives_scripts_and_rules_against_a_simulated_board() {
    use wirelab_core::circuit::PlacedComponent;
    use wirelab_core::component::CompState;
    use wirelab_core::program::{Action, Program, Rule, Trigger};

    let assets = assets_dir();
    let lib = Library::load(&assets.join("boards"), &assets.join("components"))
        .expect("assets load");
    let board = lib.board("esp32-c5-devkitc-1").expect("board").clone();

    // A scripted LED plus its series resistor, hooked up by the auto-wirer.
    let mut circuit = Circuit::new(&board.id);
    let part = |def_id: &str, pos: [f32; 2], script: Option<&str>| PlacedComponent {
        id: wirelab_core::circuit::CompId(0),
        def_id: def_id.into(),
        pos,
        rotation: 0,
        label: String::new(),
        props: Default::default(),
        state: CompState::None,
        script: script.map(str::to_string),
    };
    let led = circuit.add_component(part("led-red", [30.0, 10.0], Some("fn on_start() { me.on(); }")));
    let res = circuit.add_component(part("resistor-220", [20.0, 10.0], None));
    let plan = wirelab_core::autowire::auto_wire(&circuit, &board, &lib, &[led, res]);
    assert!(!plan.wires.is_empty(), "auto-wire found hookups");
    for (a, b) in plan.wires {
        circuit.add_wire(a, b, [200, 200, 200]);
    }

    let netlist = wirelab_core::netlist::Netlist::build(&circuit, &board, &lib);
    let (_, bindings) = wirelab_core::engine::plan_setup(&circuit, &board, &lib, &netlist);
    let led_gpio = bindings.gpio_of(led).expect("LED bound to a GPIO");
    let lints = wirelab_core::validate::validate(&circuit, &board, &lib, &netlist);
    // Any stable non-MAX keys: the runner only reacts to them changing.
    let model = BoardModel {
        lib: lib.clone(),
        board,
        netlist,
        bindings,
        lints,
        setup_key: 1,
        scripts_key: 2,
        flow_key: 3,
    };
    let tab = wirelab_core::project::BoardTab {
        id: 1,
        name: "test".into(),
        circuit,
        program: Program::default(),
        flow: Default::default(),
    };

    let (addr, stop) = spawn_board();
    let ops = NetOpsShim(NetOps::new());
    let mut link = BoardLink::default();
    link.connect(&ops, &addr);
    drive_until(&mut link, &ops, "Ready", |l| l.state == LinkState::Ready);

    let mut runner = LiveRunner::default();
    runner.start();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(6);
    let mut log = Vec::new();
    while Instant::now() < deadline && link.levels & (1 << led_gpio) == 0 {
        let now_ms = start.elapsed().as_millis() as u64;
        link.poll(&ops, now_ms as f64 / 1000.0);
        runner.tick(&ops, &mut link, &model, &tab, now_ms, &mut log);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        link.levels & (1 << led_gpio) != 0,
        "the LED script drove GPIO{led_gpio} high; log: {log:?}"
    );
    assert!(runner.scripts.errors.is_empty(), "script errors: {:?}", runner.scripts.errors);
    assert!(runner.live_output.is_some(), "solve produced live paint state");

    // Rules: on start, configure a spare pin and drive it high through the
    // engine (mode first — WriteDigital only sticks on an Output pin).
    let program = Program {
        rules: vec![Rule {
            name: "boot".into(),
            enabled: true,
            trigger: Trigger::OnStart,
            actions: vec![
                Action::SetPinMode { gpio: 4, mode: wirelab_proto::PinMode::Output },
                Action::SetPin { gpio: 4, high: true },
            ],
        }],
    };
    runner.start_program(&ops, &mut link, &program, &model, start.elapsed().as_millis() as u64);
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && link.levels & (1 << 4) == 0 {
        let now_ms = start.elapsed().as_millis() as u64;
        link.poll(&ops, now_ms as f64 / 1000.0);
        runner.tick(&ops, &mut link, &model, &tab, now_ms, &mut log);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(link.levels & (1 << 4) != 0, "the rules program drove GPIO4 high; log: {log:?}");

    runner.stop(&ops, &mut link);
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
