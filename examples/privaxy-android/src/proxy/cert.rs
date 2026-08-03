//! Per-host leaf certificates, minted on demand from the local CA and cached.

use crate::proxy::ca::{self, CertAuthority};
use http::uri::Authority;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tokio::sync::Mutex;
use uluru::LRUCache;

// Well below privaxy's desktop figure of 1000 — each entry holds a rustls ServerConfig.
const MAX_CACHED_CERTIFICATES: usize = 256;
const LEAF_VALIDITY_DAYS: i64 = 365;
// ub-common-name is 64 characters (RFC 3280); SANs carry the real name.
const MAX_COMMON_NAME_LEN: usize = 64;

#[derive(Clone)]
struct Minted {
    host: String,
    server_config: Arc<ServerConfig>,
}

struct Inner {
    cache: Mutex<LRUCache<Minted, MAX_CACHED_CERTIFICATES>>,
    issuer: Issuer<'static, KeyPair>,
    ca_certificate_der: CertificateDer<'static>,
    // One key shared by every leaf, as privaxy does: only the signature is per-host.
    leaf_key: KeyPair,
    leaf_key_der: PrivateKeyDer<'static>,
}

#[derive(Clone)]
pub struct CertCache(Arc<Inner>);

impl CertCache {
    pub fn new(authority: &CertAuthority) -> Result<Self, CertError> {
        let issuer = authority.issuer()?;
        let ca_certificate_der = ca::pem_to_der(&authority.certificate_pem)
            .ok_or(CertError::MalformedCaPem)?
            .into();

        let leaf_key = KeyPair::generate()?;
        let leaf_key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));

        Ok(Self(Arc::new(Inner {
            cache: Mutex::new(LRUCache::default()),
            issuer,
            ca_certificate_der,
            leaf_key,
            leaf_key_der,
        })))
    }

    pub async fn server_config(&self, authority: &Authority) -> Result<Arc<ServerConfig>, CertError> {
        let host = authority.host().to_owned();

        let mut cache = self.0.cache.lock().await;
        if let Some(minted) = cache.find(|minted| minted.host == host) {
            return Ok(minted.server_config.clone());
        }
        drop(cache);

        let server_config = self.mint(&host)?;
        self.0.cache.lock().await.insert(Minted {
            host,
            server_config: server_config.clone(),
        });

        Ok(server_config)
    }

    fn mint(&self, host: &str) -> Result<Arc<ServerConfig>, CertError> {
        let inner = &self.0;

        let mut params = CertificateParams::new(vec![host.to_owned()])?;

        let mut distinguished_name = DistinguishedName::new();
        let common_name = if host.len() > MAX_COMMON_NAME_LEN {
            "privaxy_cn_too_long.local"
        } else {
            host
        };
        distinguished_name.push(DnType::CommonName, common_name);
        params.distinguished_name = distinguished_name;

        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(LEAF_VALIDITY_DAYS);
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;

        let certificate = params.signed_by(&inner.leaf_key, &inner.issuer)?;

        let chain = vec![
            certificate.der().clone(),
            inner.ca_certificate_der.clone(),
        ];

        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, inner.leaf_key_der.clone_key())?;

        Ok(Arc::new(server_config))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("certificate generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("the stored CA certificate is not valid PEM")]
    MalformedCaPem,
    #[error("rustls rejected the minted certificate: {0}")]
    Rustls(#[from] rustls::Error),
}
