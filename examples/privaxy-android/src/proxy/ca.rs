//! The root certificate the proxy signs per-host certificates with.
//!
//! Generated once on first run and persisted as PEM alongside the configuration. Ported from
//! privaxy's OpenSSL implementation to rcgen: OpenSSL's `vendored` feature builds the C library
//! against the NDK sysroot, which rcgen avoids entirely.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use time::{Duration, OffsetDateTime};

const ORGANIZATION: &str = "Privaxy";
const VALIDITY_DAYS: i64 = 3650;

/// A root certificate and its private key, both PEM encoded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CertAuthority {
    pub certificate_pem: String,
    pub private_key_pem: String,
}

impl CertAuthority {
    pub fn generate() -> Result<Self, rcgen::Error> {
        // ECDSA P-256 rather than privaxy's RSA-2048: key generation is milliseconds instead of
        // seconds on a phone, and a leaf signature happens on the request path for every new host.
        let key_pair = KeyPair::generate()?;

        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CountryName, "US");
        distinguished_name.push(DnType::OrganizationName, ORGANIZATION);
        distinguished_name.push(DnType::CommonName, ORGANIZATION);

        let now = OffsetDateTime::now_utc();
        let mut params = CertificateParams::default();
        params.distinguished_name = distinguished_name;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(VALIDITY_DAYS);

        let certificate = params.self_signed(&key_pair)?;

        Ok(Self {
            certificate_pem: certificate.pem(),
            private_key_pem: key_pair.serialize_pem(),
        })
    }

    /// Re-derives the signing issuer from the stored PEM.
    pub fn issuer(&self) -> Result<Issuer<'static, KeyPair>, rcgen::Error> {
        let key_pair = KeyPair::from_pem(&self.private_key_pem)?;
        Issuer::from_ca_cert_pem(&self.certificate_pem, key_pair)
    }

    /// Colon-separated SHA-256 of the certificate DER, matching what Android shows when the
    /// certificate is inspected in Settings.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};

        let Some(der) = pem_to_der(&self.certificate_pem) else {
            return String::from("unavailable");
        };
        Sha256::digest(&der)
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

pub fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    use std::fmt::Write;

    let mut base64 = String::new();
    for line in pem.lines() {
        if line.starts_with("-----") {
            continue;
        }
        let _ = write!(base64, "{}", line.trim());
    }
    decode_base64(&base64)
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}
