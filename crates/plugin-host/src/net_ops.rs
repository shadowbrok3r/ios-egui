//! Native network ops backing `net.http.*` and `ssh.*` (feature `net`).
//!
//! Every op is non-blocking: a `request`/`connect` op returns a `u64` handle immediately and
//! the plugin polls for progress, so the host's UI thread (which drives guest frames) never
//! blocks on I/O. HTTP runs on a throwaway thread per request (ureq, blocking); each SSH
//! session owns a thread with a current-thread tokio runtime driving russh.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui_ios_plugin_abi as abi;
use abi::net::{HttpPoll, HttpRequest, HttpResponse};

mod ssh;

/// Ceiling on a buffered HTTP response body.
const MAX_HTTP_BODY: u64 = 32 << 20;
const DEFAULT_HTTP_TIMEOUT_MS: u64 = 30_000;

/// Reusable network backend. Cheap to clone the `Arc`; share one across a host's ops.
#[derive(Default)]
pub struct NetOps {
    hub: Arc<NetHub>,
}

#[derive(Default)]
struct NetHub {
    next_id: AtomicU64,
    http: Mutex<HashMap<u64, HttpSlot>>,
    ssh: Mutex<HashMap<u64, ssh::Session>>,
}

enum HttpSlot {
    Pending,
    Done(HttpResponse),
    Error(String),
}

impl NetOps {
    pub fn new() -> Self {
        NetOps::default()
    }

    /// Handle a `net.*`/`ssh.*` op. Returns `None` for ops this backend doesn't own, so the
    /// caller can fall through to its own dispatch.
    pub fn handle(&self, op: &str, payload: &[u8]) -> Option<Result<Vec<u8>, String>> {
        use abi::net::op::*;
        let r = match op {
            HTTP_REQUEST => self.http_request(payload),
            HTTP_POLL => self.http_poll(payload),
            HTTP_CANCEL => self.http_cancel(payload),
            SSH_CONNECT => self.hub.ssh_connect(payload),
            SSH_POLL => self.hub.ssh_poll(payload),
            SSH_WRITE => self.hub.ssh_write(payload),
            SSH_RESIZE => self.hub.ssh_resize(payload),
            SSH_CLOSE => self.hub.ssh_close(payload),
            _ => return None,
        };
        Some(r)
    }

    fn http_request(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let req: HttpRequest = abi::decode(payload).map_err(|e| format!("bad HttpRequest: {e}"))?;
        let id = self.hub.new_id();
        lock(&self.hub.http)?.insert(id, HttpSlot::Pending);
        let hub = Arc::clone(&self.hub);
        std::thread::Builder::new()
            .name("net-http".into())
            .spawn(move || {
                let result = perform_http(req);
                if let Ok(mut map) = hub.http.lock() {
                    // Only store if still tracked (a cancel may have removed it).
                    if let Some(slot) = map.get_mut(&id) {
                        *slot = match result {
                            Ok(r) => HttpSlot::Done(r),
                            Err(e) => HttpSlot::Error(e),
                        };
                    }
                }
            })
            .map_err(|e| format!("spawn http thread: {e}"))?;
        Ok(abi::net::id_to_bytes(id))
    }

    fn http_poll(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let id = abi::net::id_from_bytes(payload).ok_or("bad request id")?;
        let mut map = lock(&self.hub.http)?;
        // Take the slot only on a terminal state, so Done/Error is delivered exactly once.
        let poll = match map.get(&id) {
            Some(HttpSlot::Pending) => HttpPoll::Pending,
            Some(HttpSlot::Done(_)) => match map.remove(&id) {
                Some(HttpSlot::Done(r)) => HttpPoll::Done(r),
                _ => unreachable!(),
            },
            Some(HttpSlot::Error(_)) => match map.remove(&id) {
                Some(HttpSlot::Error(e)) => HttpPoll::Error(e),
                _ => unreachable!(),
            },
            None => HttpPoll::Error("unknown or completed request id".into()),
        };
        Ok(abi::encode(&poll))
    }

    fn http_cancel(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let id = abi::net::id_from_bytes(payload).ok_or("bad request id")?;
        lock(&self.hub.http)?.remove(&id);
        Ok(Vec::new())
    }
}

impl NetHub {
    fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Lock a mutex, mapping poisoning to an op error instead of panicking the UI thread.
fn lock<T>(m: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, String> {
    m.lock().map_err(|_| "network state poisoned".to_string())
}

fn perform_http(req: HttpRequest) -> Result<HttpResponse, String> {
    let timeout = Duration::from_millis(if req.timeout_ms == 0 {
        DEFAULT_HTTP_TIMEOUT_MS
    } else {
        req.timeout_ms as u64
    });
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let mut request = agent.request(&req.method, &req.url);
    for (k, v) in &req.headers {
        request = request.set(k, v);
    }
    let sent = if req.body.is_empty() {
        request.call()
    } else {
        request.send_bytes(&req.body)
    };
    match sent {
        Ok(resp) => collect_http(resp),
        // A non-2xx status is a valid response a REST client must see, not an error.
        Err(ureq::Error::Status(_, resp)) => collect_http(resp),
        Err(ureq::Error::Transport(t)) => Err(t.to_string()),
    }
}

fn collect_http(resp: ureq::Response) -> Result<HttpResponse, String> {
    let status = resp.status();
    let headers = resp
        .headers_names()
        .into_iter()
        .filter_map(|name| resp.header(&name).map(|v| (name.clone(), v.to_string())))
        .collect();
    let mut body = Vec::new();
    resp.into_reader()
        .take(MAX_HTTP_BODY)
        .read_to_end(&mut body)
        .map_err(|e| format!("reading body: {e}"))?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;

    /// Serve one canned HTTP/1.1 response, return the bound address.
    fn one_shot_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn poll_until_done(net: &NetOps, id: u64) -> HttpPoll {
        for _ in 0..200 {
            match net.http_poll(&abi::net::id_to_bytes(id)).map(|b| abi::decode::<HttpPoll>(&b).unwrap()) {
                Ok(HttpPoll::Pending) => std::thread::sleep(Duration::from_millis(10)),
                Ok(other) => return other,
                Err(e) => panic!("poll: {e}"),
            }
        }
        panic!("request never completed");
    }

    #[test]
    fn http_request_poll_delivers_once() {
        let url = one_shot_server("hello net");
        let net = NetOps::new();
        let req = HttpRequest { url, ..Default::default() };
        let id = abi::net::id_from_bytes(&net.http_request(&abi::encode(&req)).unwrap()).unwrap();

        match poll_until_done(&net, id) {
            HttpPoll::Done(resp) => {
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body, b"hello net");
            }
            other => panic!("unexpected poll result: {other:?}"),
        }
        // Terminal state is delivered exactly once; the slot is gone afterward.
        match net.http_poll(&abi::net::id_to_bytes(id)).map(|b| abi::decode::<HttpPoll>(&b).unwrap()) {
            Ok(HttpPoll::Error(_)) => {}
            other => panic!("expected error for drained id, got {other:?}"),
        }
    }
}
