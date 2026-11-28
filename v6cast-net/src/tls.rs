//! TLS 1.3 setup for QUIC: per-instance self-signed certificate on the server,
//! and a client that accepts any certificate.
//!
//! Peers are authenticated by the group token, not by certificates. The client
//! still checks the TLS 1.3 handshake signature against the presented key, so the
//! channel is encrypted with the key of whoever answered. Without certificate
//! pinning an active man-in-the-middle could relay the connection and learn the
//! token; pinning is planned for a later cycle.

use std::sync::Arc;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::error::{NetError, Result};

/// ALPN identifier of the v6cast protocol, version 1.
pub const ALPN: &[u8] = b"v6cast/1";

/// Name used for SNI and in the generated certificate.
pub const SERVER_NAME: &str = "v6cast";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// QUIC server crypto with a freshly generated self-signed certificate.
pub(crate) fn server_crypto() -> Result<QuicServerConfig> {
    let certified = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])?;
    let cert: CertificateDer<'static> = certified.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    QuicServerConfig::try_from(tls).map_err(|_| NetError::InvalidConfig("no QUIC cipher suite"))
}

/// QUIC client crypto that accepts any server certificate (see module docs).
pub(crate) fn client_crypto() -> Result<QuicClientConfig> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyServerCert { provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    QuicClientConfig::try_from(tls).map_err(|_| NetError::InvalidConfig("no QUIC cipher suite"))
}

/// Accepts every certificate but verifies handshake signatures.
#[derive(Debug)]
struct AnyServerCert {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
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
