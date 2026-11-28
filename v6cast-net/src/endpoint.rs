//! QUIC server and client over IPv6 with group-token authentication.
//!
//! Handshake: the client opens one bidirectional stream and sends
//! [`Hello`] with the group token. The server compares the token in constant time
//! and answers [`HelloAck`], or closes the connection at once with
//! [`close_code::AUTH_FAILED`]. A client that sends nothing is closed after
//! [`TransportSettings::auth_timeout`]. That stream then stays open as the
//! control channel.
//!
//! Sockets are bound IPv6-only (`IPV6_V6ONLY`), so IPv4-mapped peers never reach
//! the server.

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use quinn::{Connection, Endpoint, EndpointConfig, IdleTimeout, TokioRuntime, VarInt};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use v6cast_protocol::control::MAX_PEER_NAME_LEN;
use v6cast_protocol::{ControlMessage, Hello, HelloAck, CONTROL_VERSION};

use crate::error::{close_code, NetError, Result};
use crate::session::{read_message, write_message, Session};
use crate::tls::{self, SERVER_NAME};
use crate::token::GroupToken;

/// Timeouts and buffer sizes shared by client and server.
#[derive(Debug, Clone)]
pub struct TransportSettings {
    /// Time a peer has to complete Hello/HelloAck.
    pub auth_timeout: Duration,
    /// Connection is dropped after this long without traffic.
    pub idle_timeout: Duration,
    /// Keep-alive interval; must be below `idle_timeout`.
    pub keep_alive: Duration,
    /// Datagram buffer per direction, in bytes.
    pub datagram_buffer: usize,
}

impl Default for TransportSettings {
    fn default() -> Self {
        Self {
            auth_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(10),
            keep_alive: Duration::from_secs(2),
            datagram_buffer: 4 * 1024 * 1024,
        }
    }
}

impl TransportSettings {
    /// `peer_bidi_streams` is how many bidirectional streams the peer may open.
    fn quinn(&self, peer_bidi_streams: u32) -> Result<quinn::TransportConfig> {
        if self.keep_alive >= self.idle_timeout {
            return Err(NetError::InvalidConfig(
                "keep_alive must be below idle_timeout",
            ));
        }
        let idle = IdleTimeout::try_from(self.idle_timeout)
            .map_err(|_| NetError::InvalidConfig("idle_timeout too large"))?;
        let mut t = quinn::TransportConfig::default();
        t.max_idle_timeout(Some(idle))
            .keep_alive_interval(Some(self.keep_alive))
            .datagram_receive_buffer_size(Some(self.datagram_buffer))
            .datagram_send_buffer_size(self.datagram_buffer)
            .max_concurrent_bidi_streams(VarInt::from_u32(peer_bidi_streams))
            .max_concurrent_uni_streams(VarInt::from_u32(0));
        Ok(t)
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.len() > MAX_PEER_NAME_LEN {
        return Err(NetError::InvalidConfig("peer name longer than 64 bytes"));
    }
    Ok(())
}

/// Binds a UDP socket that only accepts IPv6 traffic.
fn bind_v6_only(addr: SocketAddrV6) -> Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(true)?;
    socket.bind(&SocketAddr::V6(addr).into())?;
    Ok(socket.into())
}

fn v6_local_addr(endpoint: &Endpoint) -> Result<SocketAddrV6> {
    match endpoint.local_addr()? {
        SocketAddr::V6(a) => Ok(a),
        SocketAddr::V4(_) => Err(NetError::InvalidConfig("endpoint is not IPv6")),
    }
}

fn close(conn: &Connection, code: u32, reason: &[u8]) {
    conn.close(VarInt::from_u32(code), reason);
}

/// Server settings.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Usually `[::]:PORT`.
    pub bind: SocketAddrV6,
    pub token: GroupToken,
    /// Name announced in HelloAck.
    pub name: String,
    pub transport: TransportSettings,
}

impl ServerConfig {
    pub fn new(bind: SocketAddrV6, token: GroupToken) -> Self {
        Self {
            bind,
            token,
            name: SERVER_NAME.to_owned(),
            transport: TransportSettings::default(),
        }
    }
}

struct AuthContext {
    token: GroupToken,
    name: String,
    auth_timeout: Duration,
    next_session: AtomicU64,
}

/// Accepts QUIC connections and yields those that authenticated.
#[derive(Debug)]
pub struct Server {
    endpoint: Endpoint,
    sessions: mpsc::Receiver<Session>,
    accept_task: JoinHandle<()>,
}

impl Server {
    /// Binds the server and starts accepting. Must run inside a Tokio runtime.
    pub fn bind(config: ServerConfig) -> Result<Self> {
        check_name(&config.name)?;
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls::server_crypto()?));
        // The client may open exactly one bidirectional stream: the control stream.
        server_config.transport_config(Arc::new(config.transport.quinn(1)?));
        let socket = bind_v6_only(config.bind)?;
        let endpoint = Endpoint::new(
            EndpointConfig::default(),
            Some(server_config),
            socket,
            Arc::new(TokioRuntime),
        )?;
        let ctx = Arc::new(AuthContext {
            token: config.token,
            name: config.name,
            auth_timeout: config.transport.auth_timeout,
            next_session: AtomicU64::new(1),
        });
        let (tx, sessions) = mpsc::channel(16);
        let accept_task = tokio::spawn(accept_loop(endpoint.clone(), ctx, tx));
        Ok(Self {
            endpoint,
            sessions,
            accept_task,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddrV6> {
        v6_local_addr(&self.endpoint)
    }

    /// Next authenticated session. `None` once the server is closed.
    pub async fn accept(&mut self) -> Option<Session> {
        self.sessions.recv().await
    }

    /// Closes every connection and stops accepting.
    pub fn close(&self) {
        self.endpoint
            .close(VarInt::from_u32(close_code::NORMAL), b"shutdown");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

async fn accept_loop(endpoint: Endpoint, ctx: Arc<AuthContext>, tx: mpsc::Sender<Session>) {
    while let Some(incoming) = endpoint.accept().await {
        let ctx = ctx.clone();
        let tx = tx.clone();
        // Each handshake runs on its own task so a slow peer cannot delay others.
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::debug!("QUIC handshake failed: {e}");
                    return;
                }
            };
            match tokio::time::timeout(ctx.auth_timeout, authenticate(&conn, &ctx)).await {
                Err(_) => close(&conn, close_code::AUTH_TIMEOUT, b"authentication timeout"),
                Ok(Err(reject)) => {
                    tracing::info!(
                        peer = %conn.remote_address(),
                        code = reject.code,
                        "rejected connection"
                    );
                    close(&conn, reject.code, reject.reason);
                }
                Ok(Ok(session)) => {
                    if tx.send(session).await.is_err() {
                        close(&conn, close_code::NORMAL, b"server stopped");
                    }
                }
            }
        });
    }
}

struct Reject {
    code: u32,
    reason: &'static [u8],
}

const AUTH_FAILED: Reject = Reject {
    code: close_code::AUTH_FAILED,
    reason: b"authentication failed",
};

const PROTOCOL_VIOLATION: Reject = Reject {
    code: close_code::PROTOCOL_VIOLATION,
    reason: b"protocol violation",
};

async fn authenticate(
    conn: &Connection,
    ctx: &AuthContext,
) -> std::result::Result<Session, Reject> {
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|_| PROTOCOL_VIOLATION)?;
    let hello = match read_message(&mut recv).await {
        Ok(ControlMessage::Hello(hello)) => hello,
        Ok(_) | Err(NetError::Protocol(_)) => return Err(PROTOCOL_VIOLATION),
        // Stream closed early or connection lost: treat as a missing token.
        Err(_) => return Err(AUTH_FAILED),
    };
    // Token first, so unauthenticated peers learn nothing about the version.
    if !ctx.token.verify(&hello.token) {
        return Err(AUTH_FAILED);
    }
    if hello.protocol_version != CONTROL_VERSION {
        return Err(Reject {
            code: close_code::VERSION_MISMATCH,
            reason: b"unsupported protocol version",
        });
    }
    let session_id = ctx.next_session.fetch_add(1, Ordering::Relaxed);
    let ack = ControlMessage::HelloAck(HelloAck {
        protocol_version: CONTROL_VERSION,
        session_id,
        peer_name: ctx.name.clone(),
    });
    write_message(&mut send, &ack)
        .await
        .map_err(|_| PROTOCOL_VIOLATION)?;
    Ok(Session::new(
        conn.clone(),
        session_id,
        hello.peer_name,
        send,
        recv,
        None,
    ))
}

/// Client settings.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Local address, usually `[::]:0`.
    pub bind: SocketAddrV6,
    pub token: GroupToken,
    /// Name announced in Hello.
    pub name: String,
    pub transport: TransportSettings,
}

impl ClientConfig {
    pub fn new(token: GroupToken) -> Self {
        Self {
            bind: SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0),
            token,
            name: SERVER_NAME.to_owned(),
            transport: TransportSettings::default(),
        }
    }
}

/// Connects to v6cast servers.
#[derive(Debug)]
pub struct Client {
    endpoint: Endpoint,
    token: GroupToken,
    name: String,
    auth_timeout: Duration,
}

impl Client {
    /// Binds the local endpoint. Must run inside a Tokio runtime.
    pub fn new(config: ClientConfig) -> Result<Self> {
        check_name(&config.name)?;
        let mut client_config = quinn::ClientConfig::new(Arc::new(tls::client_crypto()?));
        // Servers never open streams towards the client.
        client_config.transport_config(Arc::new(config.transport.quinn(0)?));
        let socket = bind_v6_only(config.bind)?;
        let mut endpoint = Endpoint::new(
            EndpointConfig::default(),
            None,
            socket,
            Arc::new(TokioRuntime),
        )?;
        endpoint.set_default_client_config(client_config);
        Ok(Self {
            endpoint,
            token: config.token,
            name: config.name,
            auth_timeout: config.transport.auth_timeout,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddrV6> {
        v6_local_addr(&self.endpoint)
    }

    /// Connects and authenticates.
    ///
    /// Returns [`NetError::AuthRejected`] when the server refuses the token.
    pub async fn connect(&self, server: SocketAddrV6) -> Result<Session> {
        let conn = self.connect_raw(server).await?;
        let hello = ControlMessage::Hello(Hello {
            protocol_version: CONTROL_VERSION,
            token: self.token.as_bytes().to_vec(),
            peer_name: self.name.clone(),
        });
        match tokio::time::timeout(self.auth_timeout, self.handshake(&conn, hello)).await {
            Ok(result) => result,
            Err(_) => {
                close(&conn, close_code::AUTH_TIMEOUT, b"authentication timeout");
                Err(NetError::AuthTimeout)
            }
        }
    }

    pub(crate) async fn connect_raw(&self, server: SocketAddrV6) -> Result<Connection> {
        self.endpoint
            .connect(SocketAddr::V6(server), SERVER_NAME)?
            .await
            .map_err(NetError::from_connection)
    }

    async fn handshake(&self, conn: &Connection, hello: ControlMessage) -> Result<Session> {
        let (mut send, mut recv) = conn.open_bi().await.map_err(NetError::from_connection)?;
        write_message(&mut send, &hello).await?;
        let ack = match read_message(&mut recv).await? {
            ControlMessage::HelloAck(ack) => ack,
            _ => {
                close(conn, close_code::PROTOCOL_VIOLATION, b"expected HelloAck");
                return Err(NetError::UnexpectedMessage("expected HelloAck"));
            }
        };
        if ack.protocol_version != CONTROL_VERSION {
            close(
                conn,
                close_code::VERSION_MISMATCH,
                b"unsupported protocol version",
            );
            return Err(NetError::UnexpectedMessage("unsupported protocol version"));
        }
        Ok(Session::new(
            conn.clone(),
            ack.session_id,
            ack.peer_name,
            send,
            recv,
            Some(self.endpoint.clone()),
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use bytes::Bytes;
    use v6cast_protocol::{FrameRate, Preset, PresetRequest};

    use super::*;

    pub(crate) fn loopback() -> SocketAddrV6 {
        SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)
    }

    pub(crate) fn token(s: &str) -> GroupToken {
        GroupToken::new(s).unwrap()
    }

    pub(crate) fn server_with(transport: TransportSettings) -> Server {
        let mut config = ServerConfig::new(loopback(), token("correct horse"));
        config.transport = transport;
        config.name = "streamer".into();
        Server::bind(config).unwrap()
    }

    pub(crate) fn client(secret: &str) -> Client {
        let mut config = ClientConfig::new(token(secret));
        config.bind = loopback();
        config.name = "viewer".into();
        Client::new(config).unwrap()
    }

    /// Server and an authenticated session on each side.
    pub(crate) async fn pair() -> (Server, Client, Session, Session) {
        let mut server = server_with(TransportSettings::default());
        let client = client("correct horse");
        let viewer = client.connect(server.local_addr().unwrap()).await.unwrap();
        let streamer = server.accept().await.unwrap();
        (server, client, streamer, viewer)
    }

    async fn close_code_of(conn: &Connection) -> u64 {
        match tokio::time::timeout(Duration::from_secs(5), conn.closed())
            .await
            .expect("connection was not closed")
        {
            quinn::ConnectionError::ApplicationClosed(c) => c.error_code.into_inner(),
            other => panic!("unexpected close: {other}"),
        }
    }

    /// Writes raw bytes on a new stream. The caller keeps the returned streams
    /// alive, so only the server can end the exchange.
    async fn send_raw(conn: &Connection, bytes: &[u8]) -> (quinn::SendStream, quinn::RecvStream) {
        let (mut send, recv) = conn.open_bi().await.unwrap();
        send.write_all(bytes).await.unwrap();
        (send, recv)
    }

    #[tokio::test]
    async fn authenticated_peers_exchange_control_and_datagrams() {
        let (server, _client, streamer, viewer) = pair().await;
        assert_eq!(streamer.session_id(), viewer.session_id());
        assert_eq!(streamer.peer_name(), "viewer");
        assert_eq!(viewer.peer_name(), "streamer");
        assert!(server.local_addr().unwrap().ip().is_loopback());
        assert!(matches!(viewer.remote_address(), SocketAddr::V6(_)));

        let request = ControlMessage::PresetRequest(PresetRequest {
            preset: Preset::P1080,
            frame_rate: FrameRate::Fps60,
        });
        viewer.send_control(&request).await.unwrap();
        assert_eq!(streamer.recv_control().await.unwrap(), request);

        streamer
            .send_datagram(Bytes::from_static(b"frame fragment"))
            .unwrap();
        assert_eq!(
            &viewer.recv_datagram().await.unwrap()[..],
            b"frame fragment"
        );
        viewer.send_datagram(Bytes::from_static(b"back")).unwrap();
        assert_eq!(&streamer.recv_datagram().await.unwrap()[..], b"back");
        assert!(viewer.max_datagram_size().unwrap() >= v6cast_protocol::IPV6_SAFE_DATAGRAM);
    }

    #[tokio::test]
    async fn wrong_token_is_rejected_immediately() {
        let mut server = server_with(TransportSettings::default());
        let intruder = client("wrong horse");
        let started = Instant::now();
        let result = intruder.connect(server.local_addr().unwrap()).await;
        assert!(matches!(result, Err(NetError::AuthRejected)), "{result:?}");
        // Closed at once, not after the 5 s authentication timeout.
        assert!(started.elapsed() < Duration::from_secs(2));
        let accepted = tokio::time::timeout(Duration::from_millis(300), server.accept()).await;
        assert!(accepted.is_err(), "rejected peer must not be accepted");
    }

    #[tokio::test]
    async fn empty_token_is_rejected() {
        let server = server_with(TransportSettings::default());
        let raw = client("unused")
            .connect_raw(server.local_addr().unwrap())
            .await
            .unwrap();
        let hello = ControlMessage::Hello(Hello {
            protocol_version: CONTROL_VERSION,
            token: Vec::new(),
            peer_name: String::new(),
        });
        let _streams = send_raw(&raw, &hello.encode_frame().unwrap()).await;
        assert_eq!(
            close_code_of(&raw).await,
            u64::from(close_code::AUTH_FAILED)
        );
    }

    #[tokio::test]
    async fn silent_peer_is_closed_after_the_auth_timeout() {
        let server = server_with(TransportSettings {
            auth_timeout: Duration::from_millis(200),
            ..TransportSettings::default()
        });
        let raw = client("unused")
            .connect_raw(server.local_addr().unwrap())
            .await
            .unwrap();
        assert_eq!(
            close_code_of(&raw).await,
            u64::from(close_code::AUTH_TIMEOUT)
        );
    }

    #[tokio::test]
    async fn non_hello_first_message_is_a_protocol_violation() {
        let server = server_with(TransportSettings::default());
        let addr = server.local_addr().unwrap();
        let request = ControlMessage::PresetRequest(PresetRequest {
            preset: Preset::P720,
            frame_rate: FrameRate::Fps30,
        });
        let cases: [Vec<u8>; 2] = [
            request.encode_frame().unwrap(),
            vec![0, 0, 0, 3, 0xee, 1, 2],
        ];
        for bytes in cases {
            let raw = client("unused").connect_raw(addr).await.unwrap();
            let _streams = send_raw(&raw, &bytes).await;
            assert_eq!(
                close_code_of(&raw).await,
                u64::from(close_code::PROTOCOL_VIOLATION)
            );
        }
    }

    #[tokio::test]
    async fn right_token_with_other_version_is_refused() {
        let server = server_with(TransportSettings::default());
        let raw = client("unused")
            .connect_raw(server.local_addr().unwrap())
            .await
            .unwrap();
        let hello = ControlMessage::Hello(Hello {
            protocol_version: CONTROL_VERSION + 1,
            token: b"correct horse".to_vec(),
            peer_name: String::new(),
        });
        let _streams = send_raw(&raw, &hello.encode_frame().unwrap()).await;
        assert_eq!(
            close_code_of(&raw).await,
            u64::from(close_code::VERSION_MISMATCH)
        );
    }

    #[tokio::test]
    async fn every_client_gets_its_own_session() {
        let mut server = server_with(TransportSettings::default());
        let addr = server.local_addr().unwrap();
        let a = client("correct horse").connect(addr).await.unwrap();
        let b = client("correct horse").connect(addr).await.unwrap();
        let s1 = server.accept().await.unwrap();
        let s2 = server.accept().await.unwrap();
        assert_ne!(a.session_id(), b.session_id());
        let mut server_ids = [s1.session_id(), s2.session_id()];
        server_ids.sort_unstable();
        let mut client_ids = [a.session_id(), b.session_id()];
        client_ids.sort_unstable();
        assert_eq!(server_ids, client_ids);
    }

    #[test]
    fn invalid_settings_are_refused() {
        let t = TransportSettings {
            keep_alive: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(5),
            ..TransportSettings::default()
        };
        assert!(t.quinn(1).is_err());
        assert!(check_name(&"x".repeat(65)).is_err());
    }
}
