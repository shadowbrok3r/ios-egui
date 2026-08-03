//! Enough DNS to read hostnames back out of the answers passing through the tun.
//!
//! Captured packets name addresses; the filter lists name hosts. [`super::sniff`] recovers the
//! host from a TLS ClientHello, which covers HTTPS. This covers the rest: every answer the tun
//! forwards records which addresses a name resolved to, so a later connection to one of those
//! addresses can be attributed even with no ClientHello to read.
//!
//! Only the question name and the A/AAAA answer addresses are parsed. A CNAME chain still maps to
//! the queried name, which is the name the app connected to and the name the rules match.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const RECORD_A: u16 = 1;
const RECORD_AAAA: u16 = 28;
const HEADER_LEN: usize = 12;
/// Longest a name may be once the labels are joined, per RFC 1035.
const MAX_NAME: usize = 255;

/// Addresses seen in DNS answers, and the name each was answered for.
pub struct DnsMap {
    entries: Mutex<Inner>,
    capacity: usize,
    ttl: Duration,
}

#[derive(Default)]
struct Inner {
    by_address: HashMap<IpAddr, (String, Instant)>,
    order: VecDeque<IpAddr>,
}

impl DnsMap {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(Inner::default()),
            capacity,
            ttl,
        }
    }

    /// The name `address` was last answered for, if it was seen recently enough.
    pub fn lookup(&self, address: IpAddr) -> Option<String> {
        let entries = self.entries.lock().ok()?;
        let (name, at) = entries.by_address.get(&address)?;
        (at.elapsed() < self.ttl).then(|| name.clone())
    }

    /// Record every A/AAAA answer in a response. Ignores anything that is not one.
    pub fn observe_response(&self, message: &[u8]) {
        let Some(answer) = parse_response(message) else {
            return;
        };
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        for address in answer.addresses {
            if entries
                .by_address
                .insert(address, (answer.name.clone(), Instant::now()))
                .is_none()
            {
                entries.order.push_back(address);
            }
            while entries.order.len() > self.capacity {
                if let Some(oldest) = entries.order.pop_front() {
                    entries.by_address.remove(&oldest);
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.by_address.len())
            .unwrap_or(0)
    }
}

/// A response's question name and the addresses it answered with.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub name: String,
    pub addresses: Vec<IpAddr>,
}

/// The question name and A/AAAA answers of a DNS response, or `None` if it is not one.
pub fn parse_response(message: &[u8]) -> Option<Answer> {
    if message.len() < HEADER_LEN {
        return None;
    }
    // QR bit: responses only.
    if message[2] & 0x80 == 0 {
        return None;
    }
    let questions = u16::from_be_bytes([message[4], message[5]]);
    let answers = u16::from_be_bytes([message[6], message[7]]);
    if questions == 0 || answers == 0 {
        return None;
    }

    let mut at = HEADER_LEN;
    let name = read_name(message, &mut at)?;
    // qtype + qclass.
    at = at.checked_add(4)?;
    // Only the first question is read; multi-question queries are not a thing in practice.
    for _ in 1..questions {
        skip_name(message, &mut at)?;
        at = at.checked_add(4)?;
    }

    let mut addresses = Vec::new();
    for _ in 0..answers {
        skip_name(message, &mut at)?;
        let kind = u16::from_be_bytes([*message.get(at)?, *message.get(at + 1)?]);
        // type + class + ttl.
        at = at.checked_add(8)?;
        let length = usize::from(u16::from_be_bytes([
            *message.get(at)?,
            *message.get(at + 1)?,
        ]));
        at = at.checked_add(2)?;
        let data = message.get(at..at.checked_add(length)?)?;
        at += length;

        match (kind, data.len()) {
            (RECORD_A, 4) => {
                addresses.push(IpAddr::V4(Ipv4Addr::new(data[0], data[1], data[2], data[3])));
            }
            (RECORD_AAAA, 16) => {
                let octets: [u8; 16] = data.try_into().ok()?;
                addresses.push(IpAddr::V6(Ipv6Addr::from(octets)));
            }
            _ => {}
        }
    }

    (!addresses.is_empty()).then_some(Answer { name, addresses })
}

/// Read a name at `at`, following compression pointers, and leave `at` past the name as written.
fn read_name(message: &[u8], at: &mut usize) -> Option<String> {
    let mut name = String::new();
    let mut cursor = *at;
    let mut jumped = false;
    // A pointer loop would otherwise spin forever; no name needs more hops than it has labels.
    let mut budget = message.len();

    loop {
        budget = budget.checked_sub(1)?;
        let length = *message.get(cursor)?;

        if length & 0xc0 == 0xc0 {
            let target = usize::from(u16::from_be_bytes([length & 0x3f, *message.get(cursor + 1)?]));
            if !jumped {
                *at = cursor + 2;
                jumped = true;
            }
            cursor = target;
            continue;
        }
        if length & 0xc0 != 0 {
            return None;
        }

        let length = usize::from(length);
        if length == 0 {
            if !jumped {
                *at = cursor + 1;
            }
            return Some(name);
        }

        let label = message.get(cursor + 1..cursor + 1 + length)?;
        if name.len() + label.len() + 1 > MAX_NAME {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(label).to_ascii_lowercase());
        cursor += 1 + length;
    }
}

/// Advance `at` past a name without building it.
fn skip_name(message: &[u8], at: &mut usize) -> Option<()> {
    read_name(message, at).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_name(name: &str, out: &mut Vec<u8>) {
        for label in name.split('.') {
            out.push(label.len() as u8);
            out.extend(label.as_bytes());
        }
        out.push(0);
    }

    /// A response with one question and the given answers, the owner name always compressed to
    /// offset 12 the way every real resolver writes it.
    fn response(name: &str, answers: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut message = vec![0x12, 0x34, 0x81, 0x80];
        message.extend(1u16.to_be_bytes());
        message.extend((answers.len() as u16).to_be_bytes());
        message.extend([0, 0, 0, 0]);
        encode_name(name, &mut message);
        message.extend(1u16.to_be_bytes()); // qtype
        message.extend(1u16.to_be_bytes()); // qclass

        for (kind, data) in answers {
            message.extend([0xc0, 0x0c]); // pointer to the question name
            message.extend(kind.to_be_bytes());
            message.extend(1u16.to_be_bytes()); // class IN
            message.extend(300u32.to_be_bytes()); // ttl
            message.extend((data.len() as u16).to_be_bytes());
            message.extend(data);
        }
        message
    }

    #[test]
    fn reads_a_records() {
        let message = response("ads.example.com", &[(RECORD_A, vec![93, 184, 216, 34])]);
        let answer = parse_response(&message).unwrap();
        assert_eq!(answer.name, "ads.example.com");
        assert_eq!(answer.addresses, vec![IpAddr::from([93, 184, 216, 34])]);
    }

    #[test]
    fn reads_aaaa_records_and_skips_cnames() {
        let cname = {
            let mut data = Vec::new();
            encode_name("edge.example.net", &mut data);
            data
        };
        let message = response(
            "www.example.com",
            &[(5, cname), (RECORD_AAAA, vec![0x20, 0x01, 0x0d, 0xb8].into_iter().chain(std::iter::repeat(0).take(11)).chain([1]).collect())],
        );
        let answer = parse_response(&message).unwrap();
        // The address maps to the queried name, not the CNAME target: that is the name the app
        // asked for and the name the rules are written against.
        assert_eq!(answer.name, "www.example.com");
        assert_eq!(answer.addresses.len(), 1);
        assert!(answer.addresses[0].is_ipv6());
    }

    #[test]
    fn queries_and_junk_are_not_responses() {
        let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        encode_name("example.com", &mut query);
        query.extend([0, 1, 0, 1]);
        assert_eq!(parse_response(&query), None);
        assert_eq!(parse_response(b""), None);
        assert_eq!(parse_response(&[0xff; 40]), None);
    }

    #[test]
    fn a_truncated_answer_does_not_panic() {
        let message = response("example.com", &[(RECORD_A, vec![1, 2, 3, 4])]);
        for cut in 0..message.len() {
            let _ = parse_response(&message[..cut]);
        }
    }

    #[test]
    fn a_pointer_loop_terminates() {
        // The question name points at itself.
        let mut message = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        message.extend([0xc0, 0x0c]);
        message.extend([0, 1, 0, 1]);
        assert_eq!(parse_response(&message), None);
    }

    #[test]
    fn the_map_evicts_oldest_first() {
        let map = DnsMap::new(2, Duration::from_secs(60));
        for (index, name) in ["a.test", "b.test", "c.test"].into_iter().enumerate() {
            let message = response(name, &[(RECORD_A, vec![10, 0, 0, index as u8])]);
            map.observe_response(&message);
        }
        assert_eq!(map.len(), 2);
        assert_eq!(map.lookup(IpAddr::from([10, 0, 0, 0])), None);
        assert_eq!(map.lookup(IpAddr::from([10, 0, 0, 2])).as_deref(), Some("c.test"));
    }

    #[test]
    fn an_expired_entry_is_not_returned() {
        let map = DnsMap::new(8, Duration::ZERO);
        map.observe_response(&response("a.test", &[(RECORD_A, vec![10, 0, 0, 1])]));
        assert_eq!(map.lookup(IpAddr::from([10, 0, 0, 1])), None);
    }
}
