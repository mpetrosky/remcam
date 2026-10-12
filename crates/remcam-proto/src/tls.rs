//! QUIC/TLS setup.
//!
//! The server uses a self-signed certificate that is generated once and kept
//! in its config directory. Clients authenticate it by SHA-256 fingerprint
//! (printed by the server at startup) rather than through a CA, the same
//! trust model as SSH host keys.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::{ALPN, SERVER_NAME};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Lowercase hex SHA-256 of a DER certificate.
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert.as_ref()))
}

/// Load the server identity from `dir`, generating and saving it on first use.
pub fn load_or_create_identity(
    dir: &Path,
) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    if cert_path.exists() && key_path.exists() {
        let cert = std::fs::read(&cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key =
            std::fs::read(&key_path).with_context(|| format!("reading {}", key_path.display()))?;
        return Ok((
            CertificateDer::from(cert),
            PrivatePkcs8KeyDer::from(key).into(),
        ));
    }

    let generated = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])?;
    let cert = generated.cert.der().clone();
    let key = generated.signing_key.serialize_der();

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&cert_path, cert.as_ref())?;
    write_private(&key_path, &key)?;
    Ok((cert, PrivatePkcs8KeyDer::from(key).into()))
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

pub fn server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
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
    /// Accept only a certificate with this SHA-256 fingerprint (hex).
    Fingerprint(String),
    /// Accept any certificate. Traffic is still encrypted, but not authenticated.
    Insecure,
}

pub fn client_config(trust: ServerTrust) -> anyhow::Result<quinn::ClientConfig> {
    if let ServerTrust::Fingerprint(fp) = &trust
        && (fp.len() != 64 || !fp.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("fingerprint must be 64 hex characters");
    }
    let provider = provider();
    let verifier = Arc::new(FingerprintVerifier {
        trust: match trust {
            ServerTrust::Fingerprint(fp) => ServerTrust::Fingerprint(fp.to_ascii_lowercase()),
            other => other,
        },
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
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
        match &self.trust {
            ServerTrust::Insecure => Ok(ServerCertVerified::assertion()),
            ServerTrust::Fingerprint(expected) if fingerprint(end_entity) == *expected => {
                Ok(ServerCertVerified::assertion())
            }
            ServerTrust::Fingerprint(_) => Err(rustls::Error::General(format!(
                "server certificate fingerprint {} does not match",
                fingerprint(end_entity)
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // Only TLS 1.3 is enabled.
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
