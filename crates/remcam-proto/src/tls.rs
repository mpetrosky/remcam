//! QUIC/TLS setup.
//!
//! Server and client each use a self-signed certificate that is generated
//! once and kept in the config directory. Each side identifies the other by
//! the SHA-256 fingerprint of its certificate rather than through a CA, the
//! same trust model as SSH keys. TLS only checks that the peer holds the key
//! for the certificate it presents; whether that fingerprint is welcome is
//! decided by the client's [`ServerTrust`] and, on the server, by the
//! application after the handshake (see `trust`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::{ALPN, SERVER_NAME};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Lowercase hex SHA-256 of a DER certificate.
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert.as_ref()))
}

/// The certificate this side presents, and its private key.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl Identity {
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }
}

/// Fingerprint of the certificate the peer presented on `conn`.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<String> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(fingerprint)
}

/// Load the identity (`cert.der`, `key.der`) from `dir`, generating and
/// saving it on first use.
pub fn load_or_create_identity(dir: &Path) -> anyhow::Result<Identity> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    if cert_path.exists() != key_path.exists() {
        bail!(
            "{} has only one of cert.der and key.der; remove it to start over",
            dir.display()
        );
    }
    if cert_path.exists() {
        let cert = std::fs::read(&cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key =
            std::fs::read(&key_path).with_context(|| format!("reading {}", key_path.display()))?;
        return Ok(Identity {
            cert: CertificateDer::from(cert),
            key: PrivatePkcs8KeyDer::from(key).into(),
        });
    }

    let generated = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])?;
    let cert = generated.cert.der().clone();
    let key = generated.signing_key.serialize_der();

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Key first: if writing the certificate fails, the next run reports the
    // half-written identity instead of silently replacing it.
    write_private(&key_path, &key)?;
    std::fs::write(&cert_path, cert.as_ref())
        .with_context(|| format!("writing {}", cert_path.display()))?;
    Ok(Identity {
        cert,
        key: PrivatePkcs8KeyDer::from(key).into(),
    })
}

fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(data)?;
    Ok(())
}

/// Server side. Every client must present a certificate; which ones are
/// allowed in is checked after the handshake against `authorized_clients`.
/// 0-RTT stays disabled (rustls' default `max_early_data_size` of 0), since
/// early data can be replayed.
pub fn server_config(identity: Identity) -> anyhow::Result<quinn::ServerConfig> {
    let provider = provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyClientCert { provider }))
        .with_single_cert(vec![identity.cert], identity.key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(15).try_into()?));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

/// How the client decides whether to trust the server's certificate.
#[derive(Debug, Clone)]
pub enum ServerTrust {
    /// Accept only a certificate with one of these SHA-256 fingerprints.
    Fingerprints(Vec<String>),
    /// Accept any certificate. Traffic is still encrypted, but the server is
    /// not authenticated. Used for pairing, which authenticates the server
    /// itself once the connection is up.
    Any,
}

/// Normalize a fingerprint typed or pasted by a person: lowercase hex,
/// `:` separators allowed.
pub fn parse_fingerprint(fp: &str) -> anyhow::Result<String> {
    let fp: String = fp.trim().chars().filter(|&c| c != ':').collect();
    if fp.len() != 64 || !fp.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("a fingerprint is 64 hex characters, got {fp:?}");
    }
    Ok(fp.to_ascii_lowercase())
}

/// Client side: present `identity`, and accept the server according to `trust`.
pub fn client_config(
    trust: ServerTrust,
    identity: Identity,
) -> anyhow::Result<quinn::ClientConfig> {
    let provider = provider();
    let verifier = Arc::new(FingerprintVerifier {
        trust,
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert], identity.key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    let mut transport = quinn::TransportConfig::default();
    // A vanished server is only noticed when the connection goes idle; keep
    // that short so the client can start reconnecting quickly.
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(Duration::from_secs(6).try_into()?));
    // Each in-flight frame occupies one uni stream; leave headroom for bursts.
    transport.max_concurrent_uni_streams(256u32.into());
    config.transport_config(Arc::new(transport));
    Ok(config)
}

#[derive(Debug)]
struct FingerprintVerifier {
    trust: ServerTrust,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity);
        match &self.trust {
            ServerTrust::Any => Ok(ServerCertVerified::assertion()),
            ServerTrust::Fingerprints(known) if known.contains(&fp) => {
                Ok(ServerCertVerified::assertion())
            }
            ServerTrust::Fingerprints(_) => Err(rustls::Error::General(format!(
                "server certificate fingerprint {fp} does not match"
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        tls12_not_offered()
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Accepts any client certificate, as long as the client proves it holds
/// the matching key (TLS does that through the handshake signature).
#[derive(Debug)]
struct AnyClientCert {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        tls12_not_offered()
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn tls12_not_offered() -> Result<HandshakeSignatureValid, rustls::Error> {
    // Only TLS 1.3 is enabled.
    Err(rustls::Error::PeerIncompatible(
        rustls::PeerIncompatible::Tls12NotOffered,
    ))
}

fn verify_tls13(
    provider: &CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}
