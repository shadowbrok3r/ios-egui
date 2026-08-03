//! Per-connection request handling: CONNECT interception, tunneling, and the filtered forward path.

use crate::proxy::blocker::FilterEngine;
use crate::proxy::cert::CertCache;
use crate::proxy::config::MitmMode;
use crate::proxy::exclusions::ExclusionStore;
use crate::proxy::state::{EventKind, Exchange, ProxyState, RequestEvent};
use bytes::Bytes;
use futures::TryStreamExt;
use http::uri::{Authority, Scheme};
use http::{HeaderMap, Uri, header};
use http_body_util::{BodyExt, Empty, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

pub type ProxyBody = BoxBody<Bytes, std::io::Error>;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(600);
// Rewriting means holding the whole document in memory; past this it is streamed through instead.
const MAX_REWRITABLE_BODY: usize = 8 * 1024 * 1024;

pub const HOP_BY_HOP_HEADERS: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub struct Session {
    pub engine: Arc<FilterEngine>,
    pub client: reqwest::Client,
    pub certs: CertCache,
    pub exclusions: ExclusionStore,
    pub intercepts: ExclusionStore,
    pub state: Arc<ProxyState>,
    pub tls_client_config: Arc<rustls::ClientConfig>,
}

pub async fn handle(
    session: Arc<Session>,
    request: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    let Some(authority) = target_authority(&request) else {
        return Ok(message_response(
            StatusCode::BAD_REQUEST,
            "Privaxy could not determine which host this request was for.",
        ));
    };

    if request.method() == Method::CONNECT {
        Ok(handle_connect(session, request, authority))
    } else {
        Ok(forward(session, request, authority, Scheme::HTTP).await)
    }
}

/// CONNECT carries only `host:port`, so this is where hostname-level blocking happens — and it is
/// the only filtering that works against apps which do not trust the local CA.
fn handle_connect(
    session: Arc<Session>,
    request: Request<Incoming>,
    authority: Authority,
) -> Response<ProxyBody> {
    let host = authority.host().to_owned();
    let logged_url = format!("https://{host}/");

    if let Some(filter) = session.engine.check_host(&host) {
        session.state.record(
            RequestEvent::now("CONNECT", &logged_url, EventKind::Blocked { filter }).note(
                "Refused as the tunnel opened, on the hostname alone. Nothing was sent, so there \
                 are no headers or body to show.",
            ),
        );
        // Refusing the tunnel surfaces as a failed connection in the client, which is what an
        // ad request should look like.
        return message_response(StatusCode::FORBIDDEN, "Blocked by Privaxy.");
    }

    // Never-intercept always wins: it is the safety valve for pinned apps, so a host on both
    // lists stays tunnelled. Otherwise the intercept list terminates a host that hostname-only
    // mode would have passed through — picking a few hosts rather than switching the whole device
    // to Full inspection, which breaks every app that does not trust a user CA.
    let excluded = session.exclusions.contains(&host);
    let intercepted = !excluded && session.intercepts.contains(&host);
    let tunnel_only = excluded || (session.state.mode() == MitmMode::HostnameOnly && !intercepted);

    // Every HTTPS connection produces one of these rows, so labelling matters: an intercepted
    // CONNECT used to be recorded as "tunneled" with a note saying TLS was *not* terminated —
    // the opposite of what happens — which is why the log reads as nothing but tunnels.
    let (kind, note) = if excluded {
        (
            EventKind::Tunneled,
            format!(
                "{host} is on the never-intercept list, so this connection is passed through byte \
                 for byte. Only the hostname is visible."
            ),
        )
    } else if tunnel_only {
        (
            EventKind::Tunneled,
            String::from(
                "TLS was tunnelled without being terminated, so only the hostname is visible. \
                 Add this host to Inspect these hosts — and install the certificate — to see \
                 inside it without switching the whole device to Full inspection.",
            ),
        )
    } else {
        (
            EventKind::Intercepted,
            String::from("Opening the connection; the requests inside are logged separately."),
        )
    };

    let exchange = session
        .state
        .record(RequestEvent::now("CONNECT", &logged_url, kind).note(note));

    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(request).await {
            Ok(upgraded) => TokioIo::new(upgraded),
            Err(error) => {
                log::debug!("CONNECT upgrade failed for {host}: {error}");
                return;
            }
        };

        if tunnel_only {
            let _ = tokio::time::timeout(TUNNEL_TIMEOUT, tunnel(upgraded, &authority)).await;
        } else {
            let _ = tokio::time::timeout(
                TUNNEL_TIMEOUT,
                intercept_tls(session, upgraded, authority, exchange),
            )
            .await;
        }
    });

    Response::new(empty_body())
}

/// Copies bytes between client and origin without looking at them.
async fn tunnel<T>(mut client: T, authority: &Authority) -> std::io::Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let port = authority.port_u16().unwrap_or(443);
    let mut origin = TcpStream::connect((authority.host(), port)).await?;
    tokio::io::copy_bidirectional(&mut client, &mut origin).await?;
    Ok(())
}

/// Terminates TLS with a certificate minted for this host, then filters the requests inside.
async fn intercept_tls<T>(
    session: Arc<Session>,
    client: T,
    authority: Authority,
    exchange: Arc<Mutex<Exchange>>,
) where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // The CONNECT row is already in the log; annotate it with whatever happens next, because
    // "opened a connection and nothing came out of it" is the single most confusing thing this
    // proxy does and the answer is almost always the certificate.
    let note = |text: String| {
        if let Ok(mut exchange) = exchange.lock() {
            exchange.note = Some(text);
        }
    };

    let server_config = match session.certs.server_config(&authority).await {
        Ok(config) => config,
        Err(error) => {
            log::warn!("No certificate for {authority}: {error}");
            note(format!("Could not mint a certificate for this host: {error}"));
            return;
        }
    };

    let tls_stream = match tokio::time::timeout(
        TLS_HANDSHAKE_TIMEOUT,
        TlsAcceptor::from(server_config).accept(client),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        // Overwhelmingly this is the client rejecting our certificate: either the CA is not
        // installed, or the app does not trust user-installed CAs (the Android 7+ default).
        Ok(Err(error)) => {
            log::debug!("TLS handshake with {authority} failed: {error}");
            note(format!(
                "This app refused Privaxy's certificate, so nothing inside the connection is \
                 visible. Either the certificate is not installed, or the app does not trust \
                 user-installed CAs — which is the default for everything except browsers since \
                 Android 7. Add {} to Never intercept to stop retrying it. ({error})",
                authority.host()
            ));
            return;
        }
        Err(_) => {
            log::debug!("TLS handshake with {authority} timed out");
            note(String::from("The TLS handshake timed out."));
            return;
        }
    };

    note(String::from(
        "TLS terminated. The requests inside are logged as their own entries.",
    ));

    let service = service_fn(move |request| {
        let session = session.clone();
        let authority = authority.clone();
        async move {
            Ok::<_, Infallible>(forward(session, request, authority, Scheme::HTTPS).await)
        }
    });

    let _ = http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(TokioIo::new(tls_stream), service)
        .with_upgrades()
        .await;
}

async fn forward(
    session: Arc<Session>,
    request: Request<Incoming>,
    authority: Authority,
    scheme: Scheme,
) -> Response<ProxyBody> {
    let Ok(uri) = Uri::builder()
        .scheme(scheme.clone())
        .authority(authority.clone())
        .path_and_query(
            request
                .uri()
                .path_and_query()
                .map(|path| path.as_str())
                .unwrap_or("/"),
        )
        .build()
    else {
        return message_response(StatusCode::BAD_REQUEST, "Malformed request URL.");
    };

    let url = uri.to_string();
    let referer = request
        .headers()
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        // Without a referer the engine treats everything as third party and over-blocks.
        .unwrap_or_else(|| url.clone());

    if let Some(filter) = session
        .engine
        .check(&url, &referer, request_type(request.headers()))
    {
        let exchange = session.state.record(
            RequestEvent::now(
                request.method().as_str(),
                &url,
                EventKind::Blocked { filter },
            )
            .note("Blocked before the request left the phone, so there is no response to show."),
        );
        if let Ok(mut exchange) = exchange.lock() {
            exchange.request_headers = header_pairs(request.headers());
        }
        return message_response(StatusCode::FORBIDDEN, "Blocked by Privaxy.");
    }

    if request.headers().contains_key(header::UPGRADE) {
        return upgrade_through(session, request, uri, scheme).await;
    }

    let exchange = session.state.record(RequestEvent::now(
        request.method().as_str(),
        &url,
        EventKind::Proxied,
    ));
    if let Ok(mut open) = exchange.lock() {
        // What the client actually sent, before hop-by-hop headers are stripped for the origin.
        open.request_headers = header_pairs(request.headers());
    }

    let method = request.method().clone();
    let mut headers = request.headers().clone();
    strip_hop_by_hop(&mut headers);
    headers.remove(header::HOST);

    // Streamed rather than collected so a large upload is not held in memory; the tee keeps a
    // bounded prefix on the way past.
    let uploaded = exchange.clone();
    let body = reqwest::Body::wrap_stream(request.into_body().into_data_stream().inspect_ok(
        move |chunk: &Bytes| {
            if let Ok(mut uploaded) = uploaded.lock() {
                uploaded.record_request_chunk(chunk);
            }
        },
    ));

    let response = match session
        .client
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            // The exchange is already logged, so without this the row shows no status and no
            // reason — a DNS failure and a refused connection look identical to an empty response.
            let message = format!("Privaxy could not reach {}: {error}", authority.host());
            note_failure(&exchange, &message);
            return message_response(StatusCode::BAD_GATEWAY, &message);
        }
    };

    let status = response.status();
    let mut response_headers = response.headers().clone();
    if let Ok(mut open) = exchange.lock() {
        open.status = Some(status.as_u16());
        // As received: content-encoding and length are stripped below, and the inspector should
        // show what the origin actually said.
        open.response_headers = header_pairs(&response_headers);
    }
    strip_hop_by_hop(&mut response_headers);
    // reqwest already decompressed the body, and rewriting changes its length.
    response_headers.remove(header::CONTENT_ENCODING);
    response_headers.remove(header::CONTENT_LENGTH);

    let is_html = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));

    let body = if is_html {
        match response.bytes().await {
            Ok(bytes) if bytes.len() <= MAX_REWRITABLE_BODY => {
                let rewritten = rewrite_html(&url, &bytes, &session);
                // The rewritten document, not the original: it is what the page actually ran.
                if let Ok(mut open) = exchange.lock() {
                    open.record_response_chunk(&rewritten);
                }
                full_body(rewritten)
            }
            Ok(bytes) => {
                if let Ok(mut open) = exchange.lock() {
                    open.record_response_chunk(&bytes);
                }
                full_body(bytes)
            }
            Err(error) => {
                return message_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Privaxy could not read the response: {error}"),
                );
            }
        }
    } else {
        stream_body(response, exchange.clone())
    };

    let mut proxied = Response::new(body);
    *proxied.status_mut() = status;
    *proxied.headers_mut() = response_headers;
    proxied
}

/// Marks a logged exchange as finished with the reason it produced no response.
fn note_failure(exchange: &Arc<std::sync::Mutex<Exchange>>, message: &str) {
    if let Ok(mut open) = exchange.lock() {
        open.note = Some(message.to_owned());
        open.finished_at = Some(chrono::Local::now());
    }
}

/// Protocol upgrades (WebSocket, mostly) cannot be filtered, so both ends are joined and the
/// bytes are passed through. Without this, terminating TLS would break every socket-based app.
async fn upgrade_through(
    session: Arc<Session>,
    mut request: Request<Incoming>,
    uri: Uri,
    scheme: Scheme,
) -> Response<ProxyBody> {
    let host = uri.host().unwrap_or_default().to_owned();
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == Scheme::HTTPS { 443 } else { 80 });

    // forward() hands upgrades over before it records, so without this a WebSocket app is not
    // partly visible in the log — it is absent. The frames themselves are opaque once joined,
    // which the note says.
    let exchange = session.state.record(
        RequestEvent::now(request.method().as_str(), &uri.to_string(), EventKind::Proxied).note(
            "Protocol upgrade — the connection is joined end to end after the handshake, so the \
             frames are not logged.",
        ),
    );
    if let Ok(mut open) = exchange.lock() {
        open.request_headers = header_pairs(request.headers());
    }

    let origin: Box<dyn Stream> = match connect_origin(
        &host,
        port,
        scheme == Scheme::HTTPS,
        &session.tls_client_config,
    )
    .await
    {
        Ok(stream) => stream,
        Err(error) => {
            let message =
                format!("Privaxy could not open an upgrade connection to {host}: {error}");
            note_failure(&exchange, &message);
            return message_response(StatusCode::BAD_GATEWAY, &message);
        }
    };

    let (mut sender, connection) =
        match hyper::client::conn::http1::handshake(TokioIo::new(origin)).await {
            Ok(pair) => pair,
            Err(error) => {
                return message_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Upgrade handshake with {host} failed: {error}"),
                );
            }
        };
    tokio::spawn(connection.with_upgrades());

    let mut upstream_request = Request::builder()
        .method(request.method().clone())
        .uri(
            uri.path_and_query()
                .map(|path| path.as_str())
                .unwrap_or("/"),
        );
    if let Some(headers) = upstream_request.headers_mut() {
        *headers = request.headers().clone();
    }
    let Ok(upstream_request) = upstream_request.body(Empty::<Bytes>::new()) else {
        note_failure(&exchange, "Malformed upgrade request.");
        return message_response(StatusCode::BAD_REQUEST, "Malformed upgrade request.");
    };

    let mut upstream_response = match sender.send_request(upstream_request).await {
        Ok(response) => response,
        Err(error) => {
            let message = format!("Upgrade request to {host} failed: {error}");
            note_failure(&exchange, &message);
            return message_response(StatusCode::BAD_GATEWAY, &message);
        }
    };

    let status = upstream_response.status();
    let headers = upstream_response.headers().clone();
    if let Ok(mut open) = exchange.lock() {
        open.status = Some(status.as_u16());
        open.response_headers = header_pairs(&headers);
        open.finished_at = Some(chrono::Local::now());
    }

    if status == StatusCode::SWITCHING_PROTOCOLS {
        tokio::spawn(async move {
            let (client, origin) = tokio::join!(
                hyper::upgrade::on(&mut request),
                hyper::upgrade::on(&mut upstream_response)
            );
            match (client, origin) {
                (Ok(client), Ok(origin)) => {
                    let mut client = TokioIo::new(client);
                    let mut origin = TokioIo::new(origin);
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut origin).await;
                }
                (client, origin) => {
                    log::debug!("Upgrade join failed: {:?} {:?}", client.err(), origin.err());
                }
            }
        });
    }

    let mut response = Response::new(empty_body());
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

async fn connect_origin(
    host: &str,
    port: u16,
    tls: bool,
    tls_config: &Arc<rustls::ClientConfig>,
) -> std::io::Result<Box<dyn Stream>> {
    let stream = TcpStream::connect((host, port)).await?;
    if !tls {
        return Ok(Box::new(stream));
    }

    let server_name = rustls_pki_types::ServerName::try_from(host.to_owned())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let stream = tokio_rustls::TlsConnector::from(tls_config.clone())
        .connect(server_name, stream)
        .await?;
    Ok(Box::new(stream))
}

/// Collects the ids and classes on the page, asks the engine which of them are ad containers, and
/// appends a stylesheet hiding them.
fn rewrite_html(url: &str, body: &Bytes, session: &Session) -> Bytes {
    use lol_html::{HtmlRewriter, Settings, element};

    let mut ids: HashSet<String> = HashSet::new();
    let mut classes: HashSet<String> = HashSet::new();
    let mut output: Vec<u8> = Vec::with_capacity(body.len() + 1024);

    {
        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![element!("*", |element| {
                    if let Some(id) = element.get_attribute("id") {
                        ids.insert(id);
                    }
                    if let Some(class) = element.get_attribute("class") {
                        classes.extend(class.split_whitespace().map(str::to_owned));
                    }
                    Ok(())
                })],
                ..Settings::default()
            },
            |chunk: &[u8]| output.extend_from_slice(chunk),
        );

        if rewriter.write(body).is_err() || rewriter.end().is_err() {
            return body.clone();
        }
    }

    let ids: Vec<String> = ids.into_iter().collect();
    let classes: Vec<String> = classes.into_iter().collect();
    let cosmetic = session.engine.cosmetic(url, &ids, &classes);

    if cosmetic.hidden_selectors.is_empty() && cosmetic.injected_script.is_none() {
        return Bytes::from(output);
    }

    session.state.note_modified_response();

    let mut appended = String::new();
    if !cosmetic.hidden_selectors.is_empty() {
        appended.push_str("<style>");
        appended.push_str(&cosmetic.hidden_selectors.join(","));
        appended.push_str("{display:none !important}</style>");
    }
    if let Some(script) = cosmetic.injected_script {
        appended.push_str("<script type=\"application/javascript\">");
        appended.push_str(&script);
        appended.push_str("</script>");
    }
    output.extend_from_slice(appended.as_bytes());

    Bytes::from(output)
}

/// The authority the request targets: absolute-form URI first, then the Host header.
fn target_authority(request: &Request<Incoming>) -> Option<Authority> {
    if let Some(authority) = request.uri().authority() {
        return Some(authority.clone());
    }

    request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse().ok())
}

/// The filter engine weighs rules by resource type, so a guess from the request's own hints beats
/// labelling everything "other".
fn request_type(headers: &HeaderMap) -> &'static str {
    if let Some(destination) = headers
        .get("sec-fetch-dest")
        .and_then(|value| value.to_str().ok())
    {
        return match destination {
            "document" => "document",
            "iframe" | "frame" | "embed" | "object" => "subdocument",
            "script" | "worker" | "sharedworker" | "serviceworker" => "script",
            "image" => "image",
            "style" => "stylesheet",
            "font" => "font",
            "audio" | "video" | "track" => "media",
            "empty" => "xmlhttprequest",
            _ => "other",
        };
    }

    match headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    {
        Some(accept) if accept.contains("text/html") => "document",
        Some(accept) if accept.contains("text/css") => "stylesheet",
        Some(accept) if accept.starts_with("image/") => "image",
        _ => "other",
    }
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
}

/// Streams the response through to the client, keeping a bounded prefix for the inspector on the
/// way past. The whole body is never held: only what fits the cap, plus a running byte count.
fn stream_body(response: reqwest::Response, exchange: Arc<Mutex<Exchange>>) -> ProxyBody {
    let stream = response
        .bytes_stream()
        .inspect_ok(move |chunk: &Bytes| {
            if let Ok(mut exchange) = exchange.lock() {
                exchange.record_response_chunk(chunk);
            }
        })
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    StreamBody::new(stream).boxed()
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value
                .to_str()
                .map(str::to_owned)
                // Header values are bytes; a non-ASCII one is shown rather than dropped.
                .unwrap_or_else(|_| String::from_utf8_lossy(value.as_bytes()).into_owned());
            (name.as_str().to_owned(), value)
        })
        .collect()
}

fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full_body(bytes: impl Into<Bytes>) -> ProxyBody {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

fn message_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let page = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Privaxy</title></head>\
         <body style=\"font-family:system-ui,sans-serif;background:#12121a;color:#e6e6ef;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
         <div style=\"text-align:center;padding:1.5rem\"><h1>Privaxy</h1><p>{message}</p></div>\
         </body></html>"
    );

    let mut response = Response::new(full_body(page));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}
