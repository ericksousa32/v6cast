//! Shared helpers for the integration tests: IPv6 loopback guard, server and
//! client builders, and a raw QUIC client that speaks the wire format by hand.
//!
//! Every test crate compiles this module on its own, so not every helper is used
//! everywhere.
#![allow(dead_code)]

use std::future::Future;
use std::io::Write;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::QuicClientConfig;
use quinn::{Connection, Endpoint, EndpointConfig, RecvStream, SendStream};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use socket2::{Domain, Protocol, Socket, Type};
use v6cast_net::tls::{ALPN, SERVER_NAME};
use v6cast_net::{Client, ClientConfig, GroupToken, Server, ServerConfig, TransportSettings};
use v6cast_protocol::control::FRAME_PREFIX_LEN;
use v6cast_protocol::ControlMessage;

pub const TOKEN: &str = "friends only, nobody else";

/// How long any single network step may take before the test fails instead of
/// hanging. Generous: nothing on loopback comes close.
pub const STEP: Duration = Duration::from_secs(10);

/// Returns `false`, after saying why, when IPv6 loopback (`[::1]`) cannot be used.
///
/// Tests start with `if !common::ipv6_or_skip() { return; }`. Set
/// `V6CAST_REQUIRE_IPV6=1` (for example in CI) to turn the skip into a failure, so
/// a missing IPv6 stack cannot hide the whole suite.
pub fn ipv6_or_skip() -> bool {
    let available = std::net::UdpSocket::bind(SocketAddr::V6(loopback())).is_ok();
    if !available {
        let message = "IPv6 loopback ([::1]) is not available in this environment";
        if std::env::var_os("V6CAST_REQUIRE_IPV6").is_some_and(|v| v != "0") {
            panic!("{message} and V6CAST_REQUIRE_IPV6 is set");
        }
        // Written straight to stderr: `eprintln!` is captured by the test harness
        // and would hide the skip from anyone reading a passing run.
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED: {message}; the test did not run"
        );
    }
    available
}

pub fn loopback() -> SocketAddrV6 {
    SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)
}

pub fn token(secret: &str) -> GroupToken {
    GroupToken::new(secret).expect("valid token")
}

pub fn server_with(transport: TransportSettings) -> Server {
    let mut config = ServerConfig::new(loopback(), token(TOKEN));
    config.name = "streamer".into();
    config.transport = transport;
    Server::bind(config).expect("bind server on [::1]")
}

pub fn server() -> Server {
    server_with(TransportSettings::default())
}

pub fn client_named(secret: &str, name: &str) -> Client {
    let mut config = ClientConfig::new(token(secret));
    config.bind = loopback();
    config.name = name.into();
    Client::new(config).expect("bind client on [::1]")
}

pub fn client(secret: &str) -> Client {
    client_named(secret, "viewer")
}

/// Fails the test instead of hanging when `fut` takes longer than [`STEP`].
pub async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, fut)
        .await
        .unwrap_or_else(|_| panic!("timed out after {STEP:?}: {what}"))
}

/// Deterministic xorshift64* generator, so tests need no `rand` dependency and
/// every failure can be replayed from its seed.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value below `n` (`n` > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

// ── raw QUIC client ──────────────────────────────────────────────────────────

/// Accepts any server certificate, like the real client, but checks signatures.
#[derive(Debug)]
struct AnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A QUIC connection that completed the TLS handshake but has sent nothing at the
/// application level. The tests write whatever bytes they want on it.
pub struct RawPeer {
    pub connection: Connection,
    // Keeps the UDP socket alive for as long as the peer exists.
    _endpoint: Endpoint,
}

impl RawPeer {
    pub async fn connect(server: SocketAddrV6) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = QuicClientConfig::try_from(tls).expect("QUIC client crypto");
        let config = quinn::ClientConfig::new(Arc::new(crypto));
        // Same socket setup as the real client. `Endpoint::client` would make the
        // socket dual-stack, which Windows refuses to bind to `[::1]` (error 10022).
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
        socket.set_only_v6(true).expect("IPV6_V6ONLY");
        socket
            .bind(&SocketAddr::V6(loopback()).into())
            .expect("bind raw endpoint on [::1]");
        let endpoint = Endpoint::new(
            EndpointConfig::default(),
            None,
            socket.into(),
            Arc::new(quinn::TokioRuntime),
        )
        .expect("raw endpoint");
        let connection = within(
            "raw QUIC handshake",
            endpoint
                .connect_with(config, SocketAddr::V6(server), SERVER_NAME)
                .expect("start handshake"),
        )
        .await
        .expect("QUIC handshake");
        Self {
            connection,
            _endpoint: endpoint,
        }
    }

    /// Opens a stream and writes `bytes` on it. The returned streams must stay
    /// alive, otherwise dropping them would end the exchange from this side.
    pub async fn send_on_new_stream(&self, bytes: &[u8]) -> (SendStream, RecvStream) {
        let (mut send, recv) = self.connection.open_bi().await.expect("open stream");
        send.write_all(bytes).await.expect("write");
        (send, recv)
    }

    /// Waits for the server to close the connection; returns the application code.
    pub async fn close_code(&self) -> u64 {
        match within("server closes the connection", self.connection.closed()).await {
            quinn::ConnectionError::ApplicationClosed(c) => c.error_code.into_inner(),
            other => panic!("expected an application close, got: {other}"),
        }
    }
}

/// Reads one complete control frame from a raw stream.
pub async fn read_control(recv: &mut RecvStream) -> ControlMessage {
    let mut prefix = [0u8; FRAME_PREFIX_LEN];
    within("control prefix", recv.read_exact(&mut prefix))
        .await
        .expect("read prefix");
    let len = ControlMessage::body_len(prefix).expect("valid length");
    let mut body = vec![0u8; len];
    within("control body", recv.read_exact(&mut body))
        .await
        .expect("read body");
    ControlMessage::decode_body(&body).expect("valid body")
}

pub fn encode(msg: &ControlMessage) -> Vec<u8> {
    msg.encode_frame().expect("encodable message")
}
