//! Turns captured IP packets into connections against the local proxy.
//!
//! [`ipstack`] runs a userspace TCP/IP stack over the tun descriptor and hands out one stream per
//! flow. Each stream is then spoken to the proxy the way a client would:
//!
//! - **port 80** — relayed byte for byte. The proxy reads origin-form requests and takes the
//!   authority from `Host`, so every request on the connection is filtered by full URL, not just
//!   by host.
//! - **everything else** — `CONNECT host:port`, then the bytes. The host comes from the TLS
//!   ClientHello where there is one, from the DNS reverse map otherwise, and finally from the
//!   destination address itself. A refusal from the proxy (a blocked host) closes the flow, which
//!   the app sees as a reset connection — the same thing it sees through the Wi-Fi proxy.
//! - **UDP** — forwarded directly, because an HTTP proxy carries no datagrams. Port 53 answers are
//!   read into the reverse map on the way past, and QUIC is optionally dropped so that the apps
//!   using it fall back to TCP, where it can be filtered.

use crate::vpn::VpnStats;
use crate::vpn::dns::DnsMap;
use crate::vpn::sniff;
use crate::vpn::tun::TunDevice;
use ipstack::{IpStack, IpStackConfig, IpStackStream, IpStackTcpStream, IpStackUdpStream};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::oneshot;

/// A phone's share of what a proxy on a laptop would take. Past this, flows are dropped rather
/// than queued: a dropped connection retries, an exhausted heap does not.
const MAX_TCP_FLOWS: usize = 512;
const MAX_UDP_FLOWS: usize = 256;

const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Long enough for the proxy to answer CONNECT, which it does without touching the network.
const PROXY_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a ClientHello before giving up on the SNI and using the reverse map.
const SNIFF_TIMEOUT: Duration = Duration::from_secs(2);
/// The proxy's CONNECT response headers; anything longer is not one.
const MAX_PROXY_REPLY: usize = 8192;

#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Where the proxy listens. Loopback, so it is never captured by the tun itself.
    pub proxy: SocketAddr,
    pub mtu: u16,
    /// Drop UDP 443 so QUIC apps fall back to TCP, where the proxy can see the traffic.
    pub block_quic: bool,
}

/// Owns the tun descriptor until `shutdown` fires or the interface goes away. Returns why it
/// stopped, or `None` when it was asked to.
pub async fn run(
    fd: OwnedFd,
    config: RelayConfig,
    stats: Arc<VpnStats>,
    dns: Arc<DnsMap>,
    mut shutdown: oneshot::Receiver<()>,
) -> Option<String> {
    let device = match TunDevice::new(fd) {
        Ok(device) => device,
        Err(error) => return Some(format!("Could not open the tun interface: {error}")),
    };

    let mut stack_config = IpStackConfig::default();
    stack_config.packet_information(false);
    stack_config.mtu_unchecked(config.mtu);
    let mut stack = IpStack::new(stack_config, device);

    let tcp_flows = Arc::new(AtomicUsize::new(0));
    let udp_flows = Arc::new(AtomicUsize::new(0));
    log::info!("Privaxy VPN relaying to {}", config.proxy);

    loop {
        let stream = tokio::select! {
            _ = &mut shutdown => return None,
            accepted = stack.accept() => match accepted {
                Ok(stream) => stream,
                Err(error) => {
                    log::warn!("VPN stack stopped: {error}");
                    return Some(format!("The VPN interface closed: {error}"));
                }
            },
        };

        match stream {
            IpStackStream::Tcp(tcp) => {
                if tcp_flows.load(Ordering::Relaxed) >= MAX_TCP_FLOWS {
                    log::warn!("VPN connection limit reached, dropping flow");
                    continue;
                }
                tcp_flows.fetch_add(1, Ordering::Relaxed);
                stats.tcp_flows.fetch_add(1, Ordering::Relaxed);

                let config = config.clone();
                let stats = stats.clone();
                let dns = dns.clone();
                let counter = tcp_flows.clone();
                tokio::spawn(async move {
                    if let Err(error) = tcp_flow(tcp, config, stats, dns).await {
                        log::debug!("VPN tcp flow ended: {error}");
                    }
                    counter.fetch_sub(1, Ordering::Relaxed);
                });
            }
            IpStackStream::Udp(udp) => {
                if udp_flows.load(Ordering::Relaxed) >= MAX_UDP_FLOWS {
                    continue;
                }
                udp_flows.fetch_add(1, Ordering::Relaxed);
                stats.udp_flows.fetch_add(1, Ordering::Relaxed);

                let config = config.clone();
                let stats = stats.clone();
                let dns = dns.clone();
                let counter = udp_flows.clone();
                tokio::spawn(async move {
                    if let Err(error) = udp_flow(udp, config, stats, dns).await {
                        log::debug!("VPN udp flow ended: {error}");
                    }
                    counter.fetch_sub(1, Ordering::Relaxed);
                });
            }
            // ICMP, IGMP and anything unparseable. Nothing here can be proxied, and answering
            // would only make the phone look like a router it is not.
            IpStackStream::UnknownTransport(_) | IpStackStream::UnknownNetwork(_) => {}
        }
    }
}

async fn tcp_flow(
    mut tcp: IpStackTcpStream,
    config: RelayConfig,
    stats: Arc<VpnStats>,
    dns: Arc<DnsMap>,
) -> std::io::Result<()> {
    let destination = tcp.peer_addr();

    // Sniffed before the proxy is dialled: a connection opened first would sit against the
    // proxy's connection limit for as long as the client took to send its hello.
    let head = if sniff::TLS_PORTS.contains(&destination.port()) {
        read_client_hello(&mut tcp).await?
    } else {
        Vec::new()
    };

    let mut upstream = match tokio::time::timeout(
        PROXY_CONNECT_TIMEOUT,
        TcpStream::connect(config.proxy),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            stats.refused.fetch_add(1, Ordering::Relaxed);
            return Err(error);
        }
        Err(_) => {
            stats.refused.fetch_add(1, Ordering::Relaxed);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the proxy did not accept the connection",
            ));
        }
    };
    let _ = upstream.set_nodelay(true);

    if destination.port() == 80 {
        // Straight through: the proxy's own HTTP parser reads the host off each request.
        copy_bidirectional(&mut tcp, &mut upstream).await?;
        return Ok(());
    }

    let host = sniff::server_name(&head)
        .or_else(|| dns.lookup(destination.ip()))
        .unwrap_or_else(|| destination.ip().to_string());

    match connect_through_proxy(&mut upstream, &host, destination.port()).await {
        Ok(leftover) => {
            if !leftover.is_empty() {
                tcp.write_all(&leftover).await?;
            }
        }
        Err(error) => {
            // A blocked host lands here. Dropping the stream resets the app's connection, which
            // is what a refused CONNECT looks like through the Wi-Fi proxy too.
            stats.refused.fetch_add(1, Ordering::Relaxed);
            log::debug!("VPN refused {host}:{}: {error}", destination.port());
            return Ok(());
        }
    }

    if !head.is_empty() {
        upstream.write_all(&head).await?;
    }
    copy_bidirectional(&mut tcp, &mut upstream).await?;
    Ok(())
}

/// Read until the ClientHello is whole, the client turns out not to be speaking TLS, or the
/// deadline passes. Whatever was read is returned so it can be replayed to the proxy.
async fn read_client_hello(tcp: &mut IpStackTcpStream) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 2048];
    let deadline = tokio::time::Instant::now() + SNIFF_TIMEOUT;

    loop {
        if !sniff::looks_like_client_hello(&head) || head.len() >= sniff::MAX_HELLO {
            return Ok(head);
        }
        if sniff::server_name(&head).is_some() {
            return Ok(head);
        }

        let read = tokio::time::timeout_at(deadline, tcp.read(&mut chunk)).await;
        match read {
            Ok(Ok(0)) => return Ok(head),
            Ok(Ok(count)) => head.extend_from_slice(&chunk[..count]),
            Ok(Err(error)) => return Err(error),
            // No hello in time — a protocol where the server speaks first, or a slow client.
            Err(_) => return Ok(head),
        }
    }
}

/// Open a tunnel through the proxy. Returns any bytes read past the response headers, which
/// already belong to the tunnel.
async fn connect_through_proxy(
    upstream: &mut TcpStream,
    host: &str,
    port: u16,
) -> std::io::Result<Vec<u8>> {
    let authority = authority(host, port);
    let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
    upstream.write_all(request.as_bytes()).await?;

    let mut reply = Vec::with_capacity(128);
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        if let Some(at) = find_header_end(&reply) {
            break at;
        }
        if reply.len() >= MAX_PROXY_REPLY {
            return Err(std::io::Error::other("the proxy's reply had no end of headers"));
        }
        match tokio::time::timeout(PROXY_REPLY_TIMEOUT, upstream.read(&mut chunk)).await {
            Ok(Ok(0)) => return Err(std::io::Error::other("the proxy closed the connection")),
            Ok(Ok(count)) => reply.extend_from_slice(&chunk[..count]),
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(std::io::Error::other("the proxy did not answer CONNECT")),
        }
    };

    let status = status_code(&reply)
        .ok_or_else(|| std::io::Error::other("the proxy's reply was not HTTP"))?;
    if !(200..300).contains(&status) {
        return Err(std::io::Error::other(format!("the proxy answered {status}")));
    }

    Ok(reply.split_off(header_end))
}

/// `host:port`, with an IPv6 literal bracketed the way a URI authority needs.
fn authority(host: &str, port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Index just past the blank line ending the response headers.
fn find_header_end(reply: &[u8]) -> Option<usize> {
    reply
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
}

fn status_code(reply: &[u8]) -> Option<u16> {
    let line = reply.split(|byte| *byte == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    if !line.starts_with("HTTP/") {
        return None;
    }
    line.split_whitespace().nth(1)?.parse().ok()
}

async fn udp_flow(
    udp: IpStackUdpStream,
    config: RelayConfig,
    stats: Arc<VpnStats>,
    dns: Arc<DnsMap>,
) -> std::io::Result<()> {
    let destination = udp.peer_addr();

    if config.block_quic && destination.port() == 443 {
        stats.quic_dropped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    // This process is excluded from the VPN, so the socket reaches the network directly instead
    // of being captured and fed back into the tun.
    let bind: SocketAddr = match destination.ip() {
        IpAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        IpAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(destination).await?;

    let is_dns = destination.port() == 53;
    let (mut from_app, mut to_app) = tokio::io::split(udp);
    let mut app_datagram = vec![0_u8; usize::from(config.mtu)];
    let mut net_datagram = vec![0_u8; 65_535];

    loop {
        tokio::select! {
            read = from_app.read(&mut app_datagram) => {
                // A read of nothing means the flow was torn down; ipstack's idle timeout errors.
                let count = read?;
                if count == 0 {
                    return Ok(());
                }
                if is_dns {
                    stats.dns_queries.fetch_add(1, Ordering::Relaxed);
                }
                socket.send(&app_datagram[..count]).await?;
            }
            received = socket.recv(&mut net_datagram) => {
                let count = received?;
                if is_dns {
                    dns.observe_response(&net_datagram[..count]);
                }
                to_app.write_all(&net_datagram[..count]).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_ipv6_authorities() {
        assert_eq!(authority("example.com", 443), "example.com:443");
        assert_eq!(authority("93.184.216.34", 443), "93.184.216.34:443");
        assert_eq!(authority("2606:4700::1111", 443), "[2606:4700::1111]:443");
    }

    #[test]
    fn reads_the_proxy_status() {
        assert_eq!(status_code(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(status_code(b"HTTP/1.1 403 Forbidden\r\n\r\n"), Some(403));
        assert_eq!(status_code(b"garbage\r\n"), None);
    }

    #[test]
    fn splits_tunnel_bytes_off_the_reply() {
        let reply = b"HTTP/1.1 200 OK\r\n\r\n\x16\x03\x01".to_vec();
        let end = find_header_end(&reply).unwrap();
        assert_eq!(&reply[end..], b"\x16\x03\x01");
        assert_eq!(find_header_end(b"HTTP/1.1 200 OK\r\n"), None);
    }
}
