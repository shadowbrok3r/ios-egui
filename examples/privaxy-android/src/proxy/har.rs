//! The request log as HAR 1.2, the format Chrome DevTools, Charles and Fiddler all import.
//!
//! Only the keys DevTools' importer actually reads are emitted, and every numeric field is a real
//! number — `-1` is the spec's "unknown", which is what most of these are: the proxy sees a
//! request and a response, not a connect/DNS/SSL breakdown.
//!
//! Bodies are the prefix the log retained, so `content.size` (everything that went past) is
//! routinely larger than `content.text` (what was kept). That mismatch is spec-legal and is what
//! `comment` records.

use crate::proxy::state::{Body, EventKind, RequestEvent};
use serde::Serialize;

#[derive(Serialize)]
pub struct Har {
    pub log: Log,
}

#[derive(Serialize)]
pub struct Log {
    pub version: &'static str,
    pub creator: Creator,
    pub entries: Vec<Entry>,
}

#[derive(Serialize)]
pub struct Creator {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Serialize)]
pub struct Entry {
    #[serde(rename = "startedDateTime")]
    pub started_date_time: String,
    pub time: f64,
    pub request: Request,
    pub response: Response,
    pub cache: Cache,
    pub timings: Timings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Serialize)]
pub struct Request {
    pub method: String,
    pub url: String,
    #[serde(rename = "httpVersion")]
    pub http_version: &'static str,
    pub cookies: Vec<()>,
    pub headers: Vec<Header>,
    #[serde(rename = "queryString")]
    pub query_string: Vec<Header>,
    #[serde(rename = "headersSize")]
    pub headers_size: i64,
    #[serde(rename = "bodySize")]
    pub body_size: i64,
    #[serde(rename = "postData", skip_serializing_if = "Option::is_none")]
    pub post_data: Option<PostData>,
}

#[derive(Serialize)]
pub struct Response {
    pub status: u16,
    #[serde(rename = "statusText")]
    pub status_text: String,
    #[serde(rename = "httpVersion")]
    pub http_version: &'static str,
    pub cookies: Vec<()>,
    pub headers: Vec<Header>,
    pub content: Content,
    #[serde(rename = "redirectURL")]
    pub redirect_url: String,
    #[serde(rename = "headersSize")]
    pub headers_size: i64,
    #[serde(rename = "bodySize")]
    pub body_size: i64,
}

#[derive(Serialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Serialize)]
pub struct Content {
    pub size: i64,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<&'static str>,
}

#[derive(Serialize)]
pub struct PostData {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub text: String,
}

#[derive(Serialize)]
pub struct Cache {}

#[derive(Serialize)]
pub struct Timings {
    pub blocked: f64,
    pub dns: f64,
    pub connect: f64,
    pub send: f64,
    pub wait: f64,
    pub receive: f64,
    pub ssl: f64,
}

pub fn build(events: &[RequestEvent]) -> Har {
    Har {
        log: Log {
            version: "1.2",
            creator: Creator {
                name: "Privaxy for Android",
                version: env!("CARGO_PKG_VERSION"),
            },
            // Oldest first: the log is newest-first, and a waterfall reads forwards.
            entries: events.iter().rev().map(entry).collect(),
        },
    }
}

fn entry(event: &RequestEvent) -> Entry {
    let exchange = event.exchange.lock().ok();
    let exchange = exchange.as_deref();

    let millis = exchange
        .and_then(|exchange| exchange.finished_at)
        .map(|finished| (finished - event.at).num_milliseconds() as f64)
        .unwrap_or(0.0)
        .max(0.0);

    let request_headers = exchange.map(|e| headers(&e.request_headers)).unwrap_or_default();
    let response_headers = exchange.map(|e| headers(&e.response_headers)).unwrap_or_default();
    let mime = exchange
        .and_then(|e| header_value(&e.response_headers, "content-type"))
        .unwrap_or_default();

    // A blocked or tunnelled entry has no status. Chrome writes 0 for a request that produced no
    // response, so the importer is happy with it and the waterfall still renders.
    let status = exchange.and_then(|e| e.status).unwrap_or(0);
    let status_text = match &event.kind {
        EventKind::Blocked { filter } => format!("Blocked by Privaxy ({filter})"),
        EventKind::Tunneled if status == 0 => String::from("Tunneled, not inspected"),
        EventKind::Intercepted if status == 0 => {
            String::from("TLS connection opened; requests inside are separate entries")
        }
        _ => String::new(),
    };

    let response_body = exchange.map(|e| &e.response_body);
    let request_body = exchange.map(|e| &e.request_body);

    Entry {
        started_date_time: event.at.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        time: millis,
        request: Request {
            method: event.method.clone(),
            url: event.url.clone(),
            http_version: "HTTP/1.1",
            cookies: Vec::new(),
            headers: request_headers,
            query_string: query_string(&event.url),
            headers_size: -1,
            body_size: request_body.map(|b| b.seen as i64).unwrap_or(-1),
            post_data: request_body.and_then(|body| {
                let text = String::from_utf8(body.bytes.clone()).ok()?;
                (!text.is_empty()).then(|| PostData {
                    mime_type: header_value(
                        exchange.map(|e| e.request_headers.as_slice()).unwrap_or(&[]),
                        "content-type",
                    )
                    .unwrap_or_else(|| String::from("application/octet-stream")),
                    text,
                })
            }),
        },
        response: Response {
            status,
            status_text,
            http_version: "HTTP/1.1",
            cookies: Vec::new(),
            headers: response_headers,
            content: content(response_body, mime),
            redirect_url: exchange
                .and_then(|e| header_value(&e.response_headers, "location"))
                .unwrap_or_default(),
            headers_size: -1,
            body_size: response_body.map(|b| b.seen as i64).unwrap_or(-1),
        },
        cache: Cache {},
        // The proxy measures one span: request in, last response byte out. Everything else is
        // honestly unknown, and -1 is how HAR says so.
        timings: Timings {
            blocked: -1.0,
            dns: -1.0,
            connect: -1.0,
            send: 0.0,
            wait: millis,
            receive: 0.0,
            ssl: -1.0,
        },
        comment: response_body.and_then(|body| {
            body.truncated().then(|| {
                format!(
                    "body truncated: {} of {} bytes retained",
                    body.bytes.len(),
                    body.seen
                )
            })
        }),
    }
}

fn content(body: Option<&Body>, mime: String) -> Content {
    let Some(body) = body else {
        return Content {
            size: 0,
            mime_type: mime,
            text: None,
            encoding: None,
        };
    };

    let (text, encoding) = if body.bytes.is_empty() {
        (None, None)
    } else {
        match std::str::from_utf8(&body.bytes) {
            Ok(text) => (Some(text.to_owned()), None),
            // A binary prefix still belongs in the capture, base64 as the spec provides for.
            Err(_) => (Some(encode_base64(&body.bytes)), Some("base64")),
        }
    };

    Content {
        size: body.seen as i64,
        mime_type: mime,
        text,
        encoding,
    }
}

fn headers(pairs: &[(String, String)]) -> Vec<Header> {
    pairs
        .iter()
        .map(|(name, value)| Header {
            name: name.clone(),
            value: value.clone(),
        })
        .collect()
}

fn header_value(pairs: &[(String, String)], name: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn query_string(url: &str) -> Vec<Header> {
    let Some((_, query)) = url.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Header {
                name: name.to_owned(),
                value: value.to_owned(),
            }
        })
        .collect()
}

fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let block = (u32::from(chunk[0]) << 16)
            | (chunk.get(1).map_or(0, |b| u32::from(*b)) << 8)
            | chunk.get(2).map_or(0, |b| u32::from(*b));
        for index in 0..4 {
            if index <= chunk.len() {
                let shift = 18 - index * 6;
                out.push(ALPHABET[((block >> shift) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::state::{EventKind, ProxyState, RequestEvent};
    use crate::proxy::config::MitmMode;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn splits_the_query_string() {
        let query = query_string("https://a.test/x?b=1&c=&d");
        assert_eq!(query.len(), 3);
        assert_eq!((query[0].name.as_str(), query[0].value.as_str()), ("b", "1"));
        assert_eq!((query[1].name.as_str(), query[1].value.as_str()), ("c", ""));
        assert_eq!((query[2].name.as_str(), query[2].value.as_str()), ("d", ""));
        assert!(query_string("https://a.test/x").is_empty());
    }

    /// Every key DevTools' importer dereferences must be present with the right JSON type.
    #[test]
    fn emits_the_keys_devtools_requires() {
        let state = ProxyState::new(MitmMode::Full);
        let exchange = state.record(RequestEvent::now(
            "GET",
            "https://example.com/a?b=1",
            EventKind::Proxied,
        ));
        {
            let mut open = exchange.lock().unwrap();
            open.status = Some(200);
            open.request_headers = vec![(String::from("host"), String::from("example.com"))];
            open.response_headers =
                vec![(String::from("content-type"), String::from("text/html"))];
            open.record_response_chunk(b"<html></html>");
        }

        let value = serde_json::to_value(build(&state.recent_events(10))).unwrap();
        let log = &value["log"];
        assert_eq!(log["version"], "1.2");
        assert!(log["creator"]["name"].is_string());
        let entry = &log["entries"][0];
        assert!(entry["startedDateTime"].is_string());
        assert!(entry["time"].is_number());
        assert!(entry["cache"].is_object());
        for key in ["blocked", "dns", "connect", "send", "wait", "receive", "ssl"] {
            assert!(entry["timings"][key].is_number(), "timings.{key}");
        }
        for key in ["headersSize", "bodySize"] {
            assert!(entry["request"][key].is_number(), "request.{key}");
            assert!(entry["response"][key].is_number(), "response.{key}");
        }
        assert!(entry["response"]["status"].is_number());
        assert!(entry["response"]["content"]["size"].is_number());
        assert_eq!(entry["response"]["content"]["text"], "<html></html>");
        assert_eq!(entry["request"]["queryString"][0]["name"], "b");
    }

    #[test]
    fn a_blocked_entry_still_produces_a_valid_response_object() {
        let state = ProxyState::new(MitmMode::HostnameOnly);
        state.record(RequestEvent::now(
            "CONNECT",
            "https://ads.example.com/",
            EventKind::Blocked {
                filter: String::from("||ads.example.com^"),
            },
        ));

        let value = serde_json::to_value(build(&state.recent_events(10))).unwrap();
        let response = &value["log"]["entries"][0]["response"];
        assert_eq!(response["status"], 0);
        assert!(
            response["statusText"]
                .as_str()
                .unwrap()
                .contains("||ads.example.com^")
        );
        assert!(response["content"]["size"].is_number());
    }

    #[test]
    fn binary_bodies_are_base64_with_the_encoding_flag() {
        let state = ProxyState::new(MitmMode::Full);
        let exchange = state.record(RequestEvent::now(
            "GET",
            "https://example.com/i.png",
            EventKind::Proxied,
        ));
        exchange
            .lock()
            .unwrap()
            .record_response_chunk(&[0x89, b'P', b'N', b'G', 0xff, 0xfe]);

        let value = serde_json::to_value(build(&state.recent_events(10))).unwrap();
        let content = &value["log"]["entries"][0]["response"]["content"];
        assert_eq!(content["encoding"], "base64");
        assert_eq!(content["text"], encode_base64(&[0x89, b'P', b'N', b'G', 0xff, 0xfe]));
    }
}
