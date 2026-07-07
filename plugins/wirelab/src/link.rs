//! Board discovery and the WireLab session, host-op backed and UI-free.
//! Everything talks through [`Ops`] so tests can run natively with a mock.

use std::collections::{BTreeMap, VecDeque};

use egui_ios_plugin_sdk::abi::{self, net};
use wirelab_proto::frame::{Decoder, encode};
use wirelab_proto::{ChipKind, DeviceMsg, HostMsg, MAX_FRAME, PROTO_VERSION};

/// The subset of the host-op surface the link needs.
pub trait Ops {
    fn call(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>, String>;
}

const HELLO_RETRY_SECS: f64 = 0.7;
const LOG_CAP: usize = 200;
const HIST_CAP: usize = 240;

// ── discovery ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Beacon {
    pub addr: String,
    pub chip: String,
    pub last_seen: f64,
}

/// Passive listener for `WIRELAB1 <ip> <port> <chip…>` UDP beacons.
#[derive(Default)]
pub struct Scanner {
    id: Option<u64>,
    boards: BTreeMap<String, Beacon>,
    pub error: Option<String>,
}

impl Scanner {
    pub fn poll(&mut self, ops: &dyn Ops, now: f64) {
        if self.error.is_some() {
            return;
        }
        let id = match self.id {
            Some(id) => id,
            None => {
                let payload = abi::encode(&net::UdpListen { port: 4519 });
                match ops.call(net::op::UDP_LISTEN, &payload) {
                    Ok(bytes) => match net::id_from_bytes(&bytes) {
                        Some(id) => {
                            self.id = Some(id);
                            id
                        }
                        None => {
                            self.error = Some("bad listen handle".into());
                            return;
                        }
                    },
                    Err(e) => {
                        self.error = Some(e);
                        return;
                    }
                }
            }
        };
        match ops.call(net::op::UDP_POLL, &net::id_to_bytes(id)) {
            Ok(bytes) => match abi::decode::<net::UdpPoll>(&bytes) {
                Ok(poll) => {
                    if let net::TcpState::Error(e) = &poll.state {
                        self.error = Some(e.clone());
                        self.id = None;
                        return;
                    }
                    for pkt in poll.packets {
                        if let Some(b) = parse_beacon(&pkt.data, now) {
                            self.boards.insert(b.addr.clone(), b);
                        }
                    }
                }
                Err(_) => self.error = Some("bad UdpPoll".into()),
            },
            Err(e) => {
                self.error = Some(e);
                self.id = None;
            }
        }
        self.boards.retain(|_, b| now - b.last_seen < 10.0);
    }

    pub fn boards(&self) -> impl Iterator<Item = &Beacon> {
        self.boards.values()
    }

    pub fn close(&mut self, ops: &dyn Ops) {
        if let Some(id) = self.id.take() {
            let _ = ops.call(net::op::UDP_CLOSE, &net::id_to_bytes(id));
        }
        self.boards.clear();
        self.error = None;
    }
}

fn parse_beacon(data: &[u8], now: f64) -> Option<Beacon> {
    let text = std::str::from_utf8(data).ok()?;
    let mut parts = text.splitn(4, ' ');
    if parts.next()? != "WIRELAB1" {
        return None;
    }
    let ip = parts.next()?;
    let port: u16 = parts.next()?.parse().ok()?;
    let chip = parts.next().unwrap_or("?").trim().to_string();
    Some(Beacon { addr: format!("{ip}:{port}"), chip, last_seen: now })
}

// ── the session ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum LinkState {
    Idle,
    /// TCP connecting, then repeating Hello until the board answers.
    Connecting,
    Ready,
    Failed(String),
}

#[derive(Clone, Copy, Debug)]
pub struct BoardInfo {
    pub chip: ChipKind,
    pub fw_version: u16,
    pub gpio_mask: u64,
    pub input_only_mask: u64,
}

/// One WireLab board over `net.tcp.*`: handshake, telemetry mirror, log.
pub struct BoardLink {
    pub state: LinkState,
    id: Option<u64>,
    dec: Decoder<DeviceMsg>,
    next_hello: f64,
    pub addr: String,
    pub info: Option<BoardInfo>,
    /// Latest digital snapshot, bit N = GPIO N.
    pub levels: u64,
    pub uptime_ms: u32,
    /// Latest analog readings and a short history per pin, millivolts.
    pub analog: BTreeMap<u8, u16>,
    pub analog_hist: BTreeMap<u8, VecDeque<u16>>,
    pub wifi: Option<(wirelab_proto::WifiState, [u8; 4])>,
    pub log: VecDeque<String>,
}

impl Default for BoardLink {
    fn default() -> Self {
        BoardLink {
            state: LinkState::Idle,
            id: None,
            dec: Decoder::new(),
            next_hello: 0.0,
            addr: String::new(),
            info: None,
            levels: 0,
            uptime_ms: 0,
            analog: BTreeMap::new(),
            analog_hist: BTreeMap::new(),
            wifi: None,
            log: VecDeque::new(),
        }
    }
}

impl BoardLink {
    pub fn connect(&mut self, ops: &dyn Ops, addr: &str) {
        self.disconnect(ops);
        let (host, port) = match addr.rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(p) => (h.to_string(), p),
                Err(_) => {
                    self.state = LinkState::Failed(format!("bad port in '{addr}'"));
                    return;
                }
            },
            None => (addr.to_string(), 4518),
        };
        let payload = abi::encode(&net::TcpConnect { host, port, timeout_ms: 5000 });
        match ops.call(net::op::TCP_CONNECT, &payload) {
            Ok(bytes) => match net::id_from_bytes(&bytes) {
                Some(id) => {
                    self.id = Some(id);
                    self.addr = addr.to_string();
                    self.state = LinkState::Connecting;
                    self.next_hello = 0.0;
                    self.push_log(format!("connecting to {addr}…"));
                }
                None => self.state = LinkState::Failed("bad connect handle".into()),
            },
            Err(e) => self.state = LinkState::Failed(e),
        }
    }

    pub fn disconnect(&mut self, ops: &dyn Ops) {
        if let Some(id) = self.id.take() {
            let _ = ops.call(net::op::TCP_CLOSE, &net::id_to_bytes(id));
        }
        *self = BoardLink { log: std::mem::take(&mut self.log), ..BoardLink::default() };
    }

    pub fn connected(&self) -> bool {
        matches!(self.state, LinkState::Connecting | LinkState::Ready)
    }

    pub fn send(&mut self, ops: &dyn Ops, msg: &HostMsg) {
        let Some(id) = self.id else { return };
        let mut buf = [0u8; MAX_FRAME];
        let Ok(n) = encode(msg, &mut buf) else { return };
        let payload = abi::encode(&net::TcpSend { id, data: buf[..n].to_vec() });
        if let Err(e) = ops.call(net::op::TCP_SEND, &payload) {
            self.push_log(format!("send failed: {e}"));
            self.state = LinkState::Failed(e);
            self.id = None;
        }
    }

    /// One pump: drain rx, decode frames, drive the handshake.
    pub fn poll(&mut self, ops: &dyn Ops, now: f64) {
        let Some(id) = self.id else { return };
        let poll = match ops.call(net::op::TCP_POLL, &net::id_to_bytes(id)) {
            Ok(bytes) => match abi::decode::<net::TcpPoll>(&bytes) {
                Ok(p) => p,
                Err(_) => {
                    self.state = LinkState::Failed("bad TcpPoll".into());
                    self.id = None;
                    return;
                }
            },
            Err(e) => {
                self.state = LinkState::Failed(e);
                self.id = None;
                return;
            }
        };
        match &poll.state {
            net::TcpState::Connecting => {}
            net::TcpState::Ready => {
                // The link is up; repeat Hello until the board answers.
                if self.state == LinkState::Connecting && now >= self.next_hello {
                    self.next_hello = now + HELLO_RETRY_SECS;
                    self.send(ops, &HostMsg::Hello { proto: PROTO_VERSION });
                }
            }
            net::TcpState::Closed(_) => {
                self.push_log("board closed the connection".into());
                self.state = LinkState::Failed("connection closed".into());
                self.id = None;
            }
            net::TcpState::Error(e) => {
                self.push_log(format!("link error: {e}"));
                self.state = LinkState::Failed(e.clone());
                self.id = None;
            }
        }
        for &b in &poll.data {
            match self.dec.push(b) {
                Some(Ok(msg)) => self.on_msg(ops, msg),
                Some(Err(_)) => self.push_log("frame decode error".into()),
                None => {}
            }
        }
    }

    fn on_msg(&mut self, ops: &dyn Ops, msg: DeviceMsg) {
        match msg {
            DeviceMsg::HelloAck { proto, fw_version, chip, gpio_mask, input_only_mask } => {
                if proto != PROTO_VERSION {
                    self.push_log(format!(
                        "protocol mismatch: app {PROTO_VERSION} vs board {proto}"
                    ));
                }
                let fresh = self.state != LinkState::Ready;
                self.info = Some(BoardInfo { chip, fw_version, gpio_mask, input_only_mask });
                self.state = LinkState::Ready;
                if fresh {
                    self.push_log(format!("hello from {}", chip.name()));
                    self.send(ops, &HostMsg::SetTelemetry { interval_ms: 50 });
                }
            }
            DeviceMsg::Telemetry { millis, levels, analog } => {
                self.uptime_ms = millis;
                self.levels = levels;
                for s in analog.iter() {
                    self.analog.insert(s.pin, s.millivolts);
                    let hist = self.analog_hist.entry(s.pin).or_default();
                    hist.push_back(s.millivolts);
                    while hist.len() > HIST_CAP {
                        hist.pop_front();
                    }
                }
            }
            DeviceMsg::AnalogValue { pin, millivolts } => {
                self.analog.insert(pin, millivolts);
            }
            DeviceMsg::Event { pin, edge, .. } => {
                self.push_log(format!("GPIO{pin} {:?}", edge));
            }
            DeviceMsg::WifiStatus { state, ip } => {
                self.wifi = Some((state, ip));
            }
            DeviceMsg::Log { msg } => self.push_log(format!("board: {msg}")),
            DeviceMsg::Error { code, pin } => {
                self.push_log(format!("board error {code:?} on pin {pin}"))
            }
            DeviceMsg::Pong { .. }
            | DeviceMsg::UartData { .. }
            | DeviceMsg::SpiData { .. }
            | DeviceMsg::I2cData { .. } => {}
        }
    }

    fn push_log(&mut self, line: String) {
        self.log.push_back(line);
        while self.log.len() > LOG_CAP {
            self.log.pop_front();
        }
    }

    /// The WS2812 data pin of the board's on-board RGB LED.
    pub fn rgb_gpio(&self) -> u8 {
        match self.info.map(|i| i.chip) {
            Some(ChipKind::Esp32C3) => 8,
            _ => 27,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Scripted op mock: canned responses per op name, recorded calls.
    struct MockOps {
        tcp_state: RefCell<net::TcpState>,
        rx: RefCell<Vec<u8>>,
        sent: RefCell<Vec<Vec<u8>>>,
    }

    impl Default for MockOps {
        fn default() -> Self {
            MockOps {
                tcp_state: RefCell::new(net::TcpState::Connecting),
                rx: RefCell::new(Vec::new()),
                sent: RefCell::new(Vec::new()),
            }
        }
    }

    impl Ops for MockOps {
        fn call(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
            match op {
                net::op::TCP_CONNECT => Ok(net::id_to_bytes(7)),
                net::op::TCP_POLL => Ok(abi::encode(&net::TcpPoll {
                    state: self.tcp_state.borrow().clone(),
                    data: std::mem::take(&mut *self.rx.borrow_mut()),
                })),
                net::op::TCP_SEND => {
                    let send: net::TcpSend = abi::decode(payload).unwrap();
                    self.sent.borrow_mut().push(send.data);
                    Ok(Vec::new())
                }
                net::op::TCP_CLOSE => Ok(Vec::new()),
                _ => Err(format!("unexpected op {op}")),
            }
        }
    }

    fn device_frame(msg: &DeviceMsg) -> Vec<u8> {
        let mut buf = [0u8; MAX_FRAME];
        let n = encode(msg, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn decode_host(frames: &[Vec<u8>]) -> Vec<HostMsg> {
        let mut dec: Decoder<HostMsg> = Decoder::new();
        let mut out = Vec::new();
        for f in frames {
            for &b in f {
                if let Some(Ok(m)) = dec.push(b) {
                    out.push(m);
                }
            }
        }
        out
    }

    #[test]
    fn handshake_reaches_ready_and_requests_telemetry() {
        let ops = MockOps::default();
        let mut link = BoardLink::default();
        link.connect(&ops, "10.0.0.5:4518");
        assert_eq!(link.state, LinkState::Connecting);

        // TCP not up yet: no hello.
        link.poll(&ops, 0.0);
        assert!(ops.sent.borrow().is_empty());

        // TCP up: hello goes out, retries until answered.
        *ops.tcp_state.borrow_mut() = net::TcpState::Ready;
        link.poll(&ops, 0.1);
        link.poll(&ops, 0.2); // within retry window: no duplicate
        assert_eq!(decode_host(&ops.sent.borrow()), vec![HostMsg::Hello { proto: PROTO_VERSION }]);
        link.poll(&ops, 1.0);
        assert_eq!(decode_host(&ops.sent.borrow()).len(), 2);

        // Board answers: link is Ready and telemetry is requested.
        ops.rx.borrow_mut().extend(device_frame(&DeviceMsg::HelloAck {
            proto: PROTO_VERSION,
            fw_version: 1,
            chip: ChipKind::Esp32C5,
            gpio_mask: 0xff,
            input_only_mask: 0,
        }));
        link.poll(&ops, 1.1);
        assert_eq!(link.state, LinkState::Ready);
        assert_eq!(link.rgb_gpio(), 27);
        let msgs = decode_host(&ops.sent.borrow());
        assert!(matches!(msgs.last(), Some(HostMsg::SetTelemetry { interval_ms: 50 })));

        // Telemetry updates the mirror.
        let mut analog = wirelab_proto::heapless::Vec::new();
        analog.push(wirelab_proto::AnalogSample { pin: 4, millivolts: 1234 }).unwrap();
        ops.rx
            .borrow_mut()
            .extend(device_frame(&DeviceMsg::Telemetry { millis: 99, levels: 0b100, analog }));
        link.poll(&ops, 1.2);
        assert_eq!(link.levels, 0b100);
        assert_eq!(link.analog.get(&4), Some(&1234));
    }

    #[test]
    fn link_error_fails_the_session() {
        let ops = MockOps::default();
        let mut link = BoardLink::default();
        link.connect(&ops, "10.0.0.5");
        *ops.tcp_state.borrow_mut() = net::TcpState::Error("refused".into());
        link.poll(&ops, 0.0);
        assert_eq!(link.state, LinkState::Failed("refused".into()));
    }

    #[test]
    fn beacons_parse_and_expire() {
        let b = parse_beacon(b"WIRELAB1 192.168.1.7 4518 ESP32-C5", 1.0).unwrap();
        assert_eq!(b.addr, "192.168.1.7:4518");
        assert_eq!(b.chip, "ESP32-C5");
        assert!(parse_beacon(b"NOPE 1 2 3", 0.0).is_none());
        assert!(parse_beacon(b"WIRELAB1 1.2.3.4 notaport chip", 0.0).is_none());
    }
}
