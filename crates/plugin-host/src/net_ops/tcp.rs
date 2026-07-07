//! Raw TCP connections and UDP datagram listeners over `std::net`.
//!
//! Each TCP connection owns a reader thread that drains the socket into a shared rx buffer
//! the `net.tcp.poll` op takes; sends go through a `try_clone` of the stream kept in the
//! slot. Each UDP listener owns a thread queuing datagrams for `net.udp.poll`. Nothing here
//! blocks the host UI thread.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui_ios_plugin_abi as abi;
use abi::net::{TcpConnect, TcpPoll, TcpSend, TcpState, UdpDatagram, UdpListen, UdpPoll};

/// Cap on rx bytes buffered between polls; past this the connection errors out.
const MAX_RX_BUFFER: usize = 1 << 20;

/// Default connect timeout when `TcpConnect::timeout_ms` is 0.
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;

/// Read timeout keeping the TCP reader loop responsive to the stop flag.
const TCP_READ_TIMEOUT: Duration = Duration::from_millis(50);

/// Read timeout keeping the UDP receive loop responsive to the stop flag.
const UDP_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// Max datagrams queued between polls; oldest dropped past this.
const MAX_UDP_QUEUE: usize = 64;

struct TcpShared {
    state: TcpState,
    rx: Vec<u8>,
}

pub(super) struct TcpConn {
    shared: Arc<Mutex<TcpShared>>,
    stop: Arc<AtomicBool>,
    /// Write half (`try_clone` of the reader's stream); `None` until connected.
    stream: Arc<Mutex<Option<TcpStream>>>,
}

struct UdpShared {
    state: TcpState,
    packets: VecDeque<UdpDatagram>,
}

pub(super) struct UdpBind {
    shared: Arc<Mutex<UdpShared>>,
    stop: Arc<AtomicBool>,
}

impl super::NetHub {
    pub(super) fn tcp_connect(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let conn: TcpConnect = abi::decode(payload).map_err(|e| format!("bad TcpConnect: {e}"))?;
        let id = self.new_id();
        let shared = Arc::new(Mutex::new(TcpShared {
            state: TcpState::Connecting,
            rx: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let stream: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
        let (shared_t, stop_t, stream_t) =
            (Arc::clone(&shared), Arc::clone(&stop), Arc::clone(&stream));
        std::thread::Builder::new()
            .name("net-tcp".into())
            .spawn(move || run_tcp_thread(conn, shared_t, stop_t, stream_t))
            .map_err(|e| format!("spawn tcp thread: {e}"))?;
        super::lock(&self.tcp)?.insert(id, TcpConn { shared, stop, stream });
        Ok(abi::net::id_to_bytes(id))
    }

    pub(super) fn tcp_poll(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let id = abi::net::id_from_bytes(payload).ok_or("bad connection id")?;
        let mut map = super::lock(&self.tcp)?;
        let poll = match map.get(&id) {
            Some(conn) => {
                let (state, data) = {
                    let mut sh = conn
                        .shared
                        .lock()
                        .map_err(|_| "tcp connection poisoned".to_string())?;
                    (sh.state.clone(), std::mem::take(&mut sh.rx))
                };
                // Once terminal and drained, forget the connection so the map can't leak.
                if state.is_terminal()
                    && let Some(conn) = map.remove(&id)
                {
                    conn.stop.store(true, Ordering::Relaxed);
                }
                TcpPoll { state, data }
            }
            None => TcpPoll {
                state: TcpState::Error("unknown or closed connection id".into()),
                data: Vec::new(),
            },
        };
        Ok(abi::encode(&poll))
    }

    pub(super) fn tcp_send(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let send: TcpSend = abi::decode(payload).map_err(|e| format!("bad TcpSend: {e}"))?;
        // Clone the stream handle out so the map lock is not held across the write.
        let stream = {
            let map = super::lock(&self.tcp)?;
            let conn = map.get(&send.id).ok_or("unknown connection id")?;
            Arc::clone(&conn.stream)
        };
        let mut guard = stream
            .lock()
            .map_err(|_| "tcp connection poisoned".to_string())?;
        match guard.as_mut() {
            Some(s) => s.write_all(&send.data).map_err(|e| format!("send: {e}"))?,
            None => return Err("not connected".into()),
        }
        Ok(Vec::new())
    }

    pub(super) fn tcp_close(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let id = abi::net::id_from_bytes(payload).ok_or("bad connection id")?;
        if let Some(conn) = super::lock(&self.tcp)?.remove(&id) {
            conn.stop.store(true, Ordering::Relaxed);
            // Shutdown unblocks the reader thread's pending read.
            if let Ok(guard) = conn.stream.lock()
                && let Some(s) = guard.as_ref()
            {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
        Ok(Vec::new())
    }

    pub(super) fn udp_listen(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let listen: UdpListen = abi::decode(payload).map_err(|e| format!("bad UdpListen: {e}"))?;
        let id = self.new_id();
        let shared = Arc::new(Mutex::new(UdpShared {
            state: TcpState::Connecting,
            packets: VecDeque::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared_t, stop_t) = (Arc::clone(&shared), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("net-udp".into())
            .spawn(move || run_udp_thread(listen.port, shared_t, stop_t))
            .map_err(|e| format!("spawn udp thread: {e}"))?;
        super::lock(&self.udp)?.insert(id, UdpBind { shared, stop });
        Ok(abi::net::id_to_bytes(id))
    }

    pub(super) fn udp_poll(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let id = abi::net::id_from_bytes(payload).ok_or("bad listener id")?;
        let mut map = super::lock(&self.udp)?;
        let poll = match map.get(&id) {
            Some(bind) => {
                let (state, packets) = {
                    let mut sh = bind
                        .shared
                        .lock()
                        .map_err(|_| "udp listener poisoned".to_string())?;
                    (sh.state.clone(), sh.packets.drain(..).collect())
                };
                // Once terminal and drained, forget the listener so the map can't leak.
                if state.is_terminal()
                    && let Some(bind) = map.remove(&id)
                {
                    bind.stop.store(true, Ordering::Relaxed);
                }
                UdpPoll { state, packets }
            }
            None => UdpPoll {
                state: TcpState::Error("unknown or closed listener id".into()),
                packets: Vec::new(),
            },
        };
        Ok(abi::encode(&poll))
    }

    pub(super) fn udp_close(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let id = abi::net::id_from_bytes(payload).ok_or("bad listener id")?;
        if let Some(bind) = super::lock(&self.udp)?.remove(&id) {
            bind.stop.store(true, Ordering::Relaxed);
        }
        Ok(Vec::new())
    }
}

fn set_tcp_state(shared: &Mutex<TcpShared>, state: TcpState) {
    if let Ok(mut sh) = shared.lock() {
        // Never overwrite a terminal state with another one (keep the first cause).
        if !sh.state.is_terminal() {
            sh.state = state;
        }
    }
}

fn set_udp_state(shared: &Mutex<UdpShared>, state: TcpState) {
    if let Ok(mut sh) = shared.lock()
        && !sh.state.is_terminal()
    {
        sh.state = state;
    }
}

/// Append to the rx buffer; `false` if the cap is exceeded.
fn push_rx(shared: &Mutex<TcpShared>, data: &[u8]) -> bool {
    match shared.lock() {
        Ok(mut sh) => {
            if sh.rx.len() + data.len() > MAX_RX_BUFFER {
                return false;
            }
            sh.rx.extend_from_slice(data);
            true
        }
        Err(_) => false,
    }
}

fn run_tcp_thread(
    conn: TcpConnect,
    shared: Arc<Mutex<TcpShared>>,
    stop: Arc<AtomicBool>,
    stream_slot: Arc<Mutex<Option<TcpStream>>>,
) {
    let timeout = Duration::from_millis(if conn.timeout_ms == 0 {
        DEFAULT_CONNECT_TIMEOUT_MS
    } else {
        conn.timeout_ms as u64
    });
    let addr = match (conn.host.as_str(), conn.port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(a) => a,
            None => {
                set_tcp_state(&shared, TcpState::Error(format!("no address for {}", conn.host)));
                return;
            }
        },
        Err(e) => {
            set_tcp_state(&shared, TcpState::Error(format!("resolve: {e}")));
            return;
        }
    };
    let mut stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(e) => {
            set_tcp_state(&shared, TcpState::Error(format!("connect: {e}")));
            return;
        }
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(TCP_READ_TIMEOUT));
    match stream.try_clone() {
        Ok(clone) => {
            if let Ok(mut slot) = stream_slot.lock() {
                *slot = Some(clone);
            }
        }
        Err(e) => {
            set_tcp_state(&shared, TcpState::Error(format!("clone stream: {e}")));
            return;
        }
    }
    set_tcp_state(&shared, TcpState::Ready);

    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::Relaxed) {
        match stream.read(&mut buf) {
            Ok(0) => {
                set_tcp_state(&shared, TcpState::Closed(String::new()));
                return;
            }
            Ok(n) => {
                if !push_rx(&shared, &buf[..n]) {
                    set_tcp_state(&shared, TcpState::Error("rx overflow".into()));
                    return;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => {
                set_tcp_state(&shared, TcpState::Error(e.to_string()));
                return;
            }
        }
    }
}

fn run_udp_thread(port: u16, shared: Arc<Mutex<UdpShared>>, stop: Arc<AtomicBool>) {
    let socket = match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            set_udp_state(&shared, TcpState::Error(format!("bind: {e}")));
            return;
        }
    };
    let _ = socket.set_read_timeout(Some(UDP_READ_TIMEOUT));
    set_udp_state(&shared, TcpState::Ready);

    // Largest possible UDP payload.
    let mut buf = vec![0u8; 64 * 1024];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Ok(mut sh) = shared.lock() {
                    if sh.packets.len() >= MAX_UDP_QUEUE {
                        sh.packets.pop_front();
                    }
                    sh.packets.push_back(UdpDatagram {
                        from: from.to_string(),
                        data: buf[..n].to_vec(),
                    });
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => {
                set_udp_state(&shared, TcpState::Error(e.to_string()));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net_ops::NetOps;
    use std::net::TcpListener;
    use std::time::SystemTime;

    fn op(net: &NetOps, name: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        net.handle(name, payload).expect("op not handled")
    }

    fn tcp_poll_once(net: &NetOps, id: u64) -> TcpPoll {
        let bytes = op(net, abi::net::op::TCP_POLL, &abi::net::id_to_bytes(id)).unwrap();
        abi::decode(&bytes).unwrap()
    }

    fn udp_poll_once(net: &NetOps, id: u64) -> UdpPoll {
        let bytes = op(net, abi::net::op::UDP_POLL, &abi::net::id_to_bytes(id)).unwrap();
        abi::decode(&bytes).unwrap()
    }

    fn tcp_connect(net: &NetOps, host: &str, port: u16, timeout_ms: u32) -> u64 {
        let conn = TcpConnect { host: host.into(), port, timeout_ms };
        let bytes = op(net, abi::net::op::TCP_CONNECT, &abi::encode(&conn)).unwrap();
        abi::net::id_from_bytes(&bytes).unwrap()
    }

    fn wait_tcp_ready(net: &NetOps, id: u64) {
        for _ in 0..200 {
            match tcp_poll_once(net, id).state {
                TcpState::Ready => return,
                TcpState::Connecting => std::thread::sleep(Duration::from_millis(10)),
                other => panic!("unexpected state: {other:?}"),
            }
        }
        panic!("connection never became ready");
    }

    /// Bind a UDP listener op on a free high port; returns (id, port).
    fn udp_listen_ready(net: &NetOps) -> (u64, u16) {
        for attempt in 0u64..20 {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64;
            let port = 20000 + (nanos.wrapping_mul(2654435761).wrapping_add(attempt * 7919) % 40000) as u16;
            let bytes = op(net, abi::net::op::UDP_LISTEN, &abi::encode(&UdpListen { port })).unwrap();
            let id = abi::net::id_from_bytes(&bytes).unwrap();
            for _ in 0..200 {
                match udp_poll_once(net, id).state {
                    TcpState::Ready => return (id, port),
                    TcpState::Connecting => std::thread::sleep(Duration::from_millis(10)),
                    // Port taken; the slot is already dropped — try another port.
                    _ => break,
                }
            }
        }
        panic!("no free udp port found");
    }

    #[test]
    fn tcp_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 256];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        let net = NetOps::new();
        let id = tcp_connect(&net, "127.0.0.1", addr.port(), 0);
        wait_tcp_ready(&net, id);

        let send = TcpSend { id, data: b"ping".to_vec() };
        op(&net, abi::net::op::TCP_SEND, &abi::encode(&send)).unwrap();

        let mut got = Vec::new();
        for _ in 0..200 {
            let poll = tcp_poll_once(&net, id);
            got.extend_from_slice(&poll.data);
            if got == b"ping" {
                break;
            }
            assert!(!poll.state.is_terminal(), "terminal before echo: {:?}", poll.state);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got, b"ping");

        op(&net, abi::net::op::TCP_CLOSE, &abi::net::id_to_bytes(id)).unwrap();
        // The slot is gone after close.
        match tcp_poll_once(&net, id).state {
            TcpState::Error(_) => {}
            other => panic!("expected error for closed id, got {other:?}"),
        }
    }

    #[test]
    fn tcp_connect_refused() {
        // Bind then drop a listener to get a port that is closed.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let net = NetOps::new();
        let id = tcp_connect(&net, "127.0.0.1", port, 2000);
        for _ in 0..200 {
            match tcp_poll_once(&net, id).state {
                TcpState::Error(_) => return,
                TcpState::Connecting => std::thread::sleep(Duration::from_millis(10)),
                other => panic!("unexpected state: {other:?}"),
            }
        }
        panic!("connection never errored");
    }

    #[test]
    fn udp_receives_datagram() {
        let net = NetOps::new();
        let (id, port) = udp_listen_ready(&net);

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        for i in 0..200 {
            // Resend periodically in case a datagram is dropped.
            if i % 20 == 0 {
                sender.send_to(b"beacon", ("127.0.0.1", port)).unwrap();
            }
            let poll = udp_poll_once(&net, id);
            assert!(!poll.state.is_terminal(), "terminal state: {:?}", poll.state);
            if let Some(pkt) = poll.packets.first() {
                assert_eq!(pkt.data, b"beacon");
                assert_eq!(pkt.from, sender.local_addr().unwrap().to_string());
                op(&net, abi::net::op::UDP_CLOSE, &abi::net::id_to_bytes(id)).unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("datagram never arrived");
    }

    #[test]
    fn udp_port_conflict_errors() {
        let net = NetOps::new();
        let (first, port) = udp_listen_ready(&net);

        let bytes = op(&net, abi::net::op::UDP_LISTEN, &abi::encode(&UdpListen { port })).unwrap();
        let second = abi::net::id_from_bytes(&bytes).unwrap();
        for _ in 0..200 {
            match udp_poll_once(&net, second).state {
                TcpState::Error(_) => {
                    op(&net, abi::net::op::UDP_CLOSE, &abi::net::id_to_bytes(first)).unwrap();
                    return;
                }
                TcpState::Connecting => std::thread::sleep(Duration::from_millis(10)),
                other => panic!("unexpected state: {other:?}"),
            }
        }
        panic!("second bind never errored");
    }
}
