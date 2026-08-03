//! TLS ClientHello parsing, for the one hostname a captured packet stream carries.
//!
//! Captured IP packets name a destination address, not a host, and the filter lists are written
//! against hosts. The SNI extension is the authoritative answer where it exists: it survives an
//! IP-literal connection, several sites behind one address, and a resolver the app reached over
//! DoH before capture started. [`super::dns`] covers what has no ClientHello.

/// Ports worth waiting on the client's first bytes for. Everything else either speaks a protocol
/// where the server goes first — waiting would stall the flow until the timeout — or is answered
/// well enough by the DNS reverse map.
pub const TLS_PORTS: [u16; 2] = [443, 8443];

/// Bytes to hold while looking for a ClientHello. A hello runs past one segment once it carries a
/// post-quantum key share, but not past this.
pub const MAX_HELLO: usize = 8192;

/// How far into `data` a ClientHello could still be completed, or `None` if these bytes cannot be
/// the start of one.
pub fn looks_like_client_hello(data: &[u8]) -> bool {
    // Handshake record, TLS 1.x major version, and the ClientHello handshake type.
    match data {
        [] => true,
        [0x16] => true,
        [0x16, 0x03] => true,
        [0x16, 0x03, _] | [0x16, 0x03, _, _] | [0x16, 0x03, _, _, _] => true,
        [0x16, 0x03, _, _, _, 0x01, ..] => true,
        _ => false,
    }
}

/// The `server_name` extension's host, if `data` holds a complete ClientHello.
///
/// Returns `None` both for "not a ClientHello" and for "not all of it yet"; the caller keeps
/// reading until [`MAX_HELLO`] or its deadline. Hostnames are lowercased, and a trailing root dot
/// is dropped, so they compare against filter rules the way the proxy's own hosts do.
pub fn server_name(data: &[u8]) -> Option<String> {
    let mut reader = Reader::new(data);

    if reader.u8()? != 0x16 {
        return None;
    }
    reader.skip(2)?; // Record version.
    let record_len = reader.u16()? as usize;
    // The hello has to be whole: a truncated extension list reads as "no SNI" rather than "wait".
    let mut body = Reader::new(reader.take(record_len)?);

    if body.u8()? != 0x01 {
        return None;
    }
    let handshake_len = body.u24()?;
    let mut hello = Reader::new(body.take(handshake_len)?);

    hello.skip(2)?; // client_version
    hello.skip(32)?; // random
    let session_len = hello.u8()? as usize;
    hello.skip(session_len)?;
    let cipher_len = hello.u16()? as usize;
    hello.skip(cipher_len)?;
    let compression_len = hello.u8()? as usize;
    hello.skip(compression_len)?;

    let extensions_len = hello.u16()? as usize;
    let mut extensions = Reader::new(hello.take(extensions_len)?);

    while !extensions.is_empty() {
        let kind = extensions.u16()?;
        let len = extensions.u16()? as usize;
        let payload = extensions.take(len)?;
        if kind != 0x0000 {
            continue;
        }

        let mut names = Reader::new(payload);
        let list_len = names.u16()? as usize;
        let mut list = Reader::new(names.take(list_len)?);
        while !list.is_empty() {
            let name_type = list.u8()?;
            let name_len = list.u16()? as usize;
            let name = list.take(name_len)?;
            // 0 is host_name; nothing else was ever assigned.
            if name_type == 0 {
                let name = std::str::from_utf8(name).ok()?;
                return Some(normalize(name));
            }
        }
    }

    None
}

/// Lowercased, with the root label's trailing dot removed.
fn normalize(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Big-endian cursor that yields `None` rather than panicking past the end.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.data.len() < len {
            return None;
        }
        let (head, rest) = self.data.split_at(len);
        self.data = rest;
        Some(head)
    }

    fn skip(&mut self, len: usize) -> Option<()> {
        self.take(len).map(|_| ())
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2)
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u24(&mut self) -> Option<usize> {
        self.take(3).map(|bytes| {
            usize::from(bytes[0]) << 16 | usize::from(bytes[1]) << 8 | usize::from(bytes[2])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_hello(host: &str) -> Vec<u8> {
        client_hello_with_extension(host, 0x0000)
    }

    /// A ClientHello carrying one extension of `kind`, holding a `server_name` list.
    fn client_hello_with_extension(host: &str, kind: u16) -> Vec<u8> {
        let mut sni = Vec::new();
        sni.push(0); // host_name
        sni.extend((host.len() as u16).to_be_bytes());
        sni.extend(host.as_bytes());

        let mut list = Vec::new();
        list.extend((sni.len() as u16).to_be_bytes());
        list.extend(&sni);

        let mut extensions = Vec::new();
        extensions.extend(kind.to_be_bytes());
        extensions.extend((list.len() as u16).to_be_bytes());
        extensions.extend(&list);

        let mut hello = Vec::new();
        hello.extend([0x03, 0x03]); // client_version
        hello.extend([0u8; 32]); // random
        hello.push(0); // session id
        hello.extend(2u16.to_be_bytes()); // cipher suites
        hello.extend([0x13, 0x01]);
        hello.push(1); // compression methods
        hello.push(0);
        hello.extend((extensions.len() as u16).to_be_bytes());
        hello.extend(&extensions);

        let mut handshake = Vec::new();
        handshake.push(0x01);
        let len = hello.len();
        handshake.extend([(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        handshake.extend(&hello);

        let mut record = Vec::new();
        record.extend([0x16, 0x03, 0x01]);
        record.extend((handshake.len() as u16).to_be_bytes());
        record.extend(&handshake);
        record
    }

    #[test]
    fn reads_the_server_name() {
        let hello = client_hello("Example.COM");
        assert_eq!(server_name(&hello).as_deref(), Some("example.com"));
    }

    #[test]
    fn drops_the_root_label() {
        let hello = client_hello("cdn.example.com.");
        assert_eq!(server_name(&hello).as_deref(), Some("cdn.example.com"));
    }

    #[test]
    fn a_truncated_hello_reads_as_none_but_still_looks_like_one() {
        let hello = client_hello("example.com");
        for cut in [1, 5, 10, hello.len() - 1] {
            assert_eq!(server_name(&hello[..cut]), None, "cut at {cut}");
        }
        assert!(looks_like_client_hello(&hello[..6]));
    }

    #[test]
    fn non_tls_is_rejected_immediately() {
        assert!(!looks_like_client_hello(b"GET / HTTP/1.1\r\n"));
        assert!(!looks_like_client_hello(b"SSH-2.0-OpenSSH"));
        assert_eq!(server_name(b"GET / HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn a_hello_without_sni_is_none() {
        // 0x000a is supported_groups; the walk has to step over it and run out of extensions.
        let hello = client_hello_with_extension("example.com", 0x000a);
        assert_eq!(server_name(&hello), None);
    }
}
