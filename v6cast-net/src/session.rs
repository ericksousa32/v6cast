//! An authenticated connection: reliable control stream plus unreliable datagrams.

use std::net::SocketAddr;

use bytes::Bytes;
use quinn::{Connection, Endpoint, ReadError, ReadExactError, RecvStream, SendStream, VarInt};
use tokio::sync::Mutex;
use v6cast_protocol::control::FRAME_PREFIX_LEN;
use v6cast_protocol::ControlMessage;

use crate::error::{close_code, NetError, Result};

/// A peer that passed the group-token check.
///
/// The control stream is the bidirectional stream that carried the Hello. Media
/// goes over QUIC datagrams, which are never retransmitted.
#[derive(Debug)]
pub struct Session {
    connection: Connection,
    session_id: u64,
    peer_name: String,
    send: Mutex<SendStream>,
    recv: Mutex<RecvStream>,
    /// Client sessions keep their endpoint alive.
    _endpoint: Option<Endpoint>,
}

impl Session {
    pub(crate) fn new(
        connection: Connection,
        session_id: u64,
        peer_name: String,
        send: SendStream,
        recv: RecvStream,
        endpoint: Option<Endpoint>,
    ) -> Self {
        Self {
            connection,
            session_id,
            peer_name,
            send: Mutex::new(send),
            recv: Mutex::new(recv),
            _endpoint: endpoint,
        }
    }

    /// Identifier chosen by the accepting side, shared by both ends.
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    pub fn peer_name(&self) -> &str {
        &self.peer_name
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// The underlying QUIC connection, for statistics.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Sends a control message on the reliable stream.
    pub async fn send_control(&self, msg: &ControlMessage) -> Result<()> {
        write_message(&mut *self.send.lock().await, msg).await
    }

    /// Waits for the next control message.
    pub async fn recv_control(&self) -> Result<ControlMessage> {
        read_message(&mut *self.recv.lock().await).await
    }

    /// Queues a datagram without waiting. Fails when it exceeds
    /// [`Session::max_datagram_size`] or the connection is gone.
    pub fn send_datagram(&self, datagram: Bytes) -> Result<()> {
        self.connection.send_datagram(datagram)?;
        Ok(())
    }

    /// Waits for the next datagram.
    pub async fn recv_datagram(&self) -> Result<Bytes> {
        self.connection
            .read_datagram()
            .await
            .map_err(NetError::from_connection)
    }

    /// Largest datagram the path accepts right now, if datagrams are enabled.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.connection.max_datagram_size()
    }

    /// Closes the connection normally.
    pub fn close(&self) {
        self.connection
            .close(VarInt::from_u32(close_code::NORMAL), b"bye");
    }

    /// Resolves when the connection ends, with the reason.
    pub async fn closed(&self) -> NetError {
        NetError::from_connection(self.connection.closed().await)
    }
}

pub(crate) async fn write_message(send: &mut SendStream, msg: &ControlMessage) -> Result<()> {
    let frame = msg.encode_frame()?;
    send.write_all(&frame).await.map_err(|e| match e {
        quinn::WriteError::ConnectionLost(c) => NetError::from_connection(c),
        _ => NetError::ControlClosed,
    })
}

pub(crate) async fn read_message(recv: &mut RecvStream) -> Result<ControlMessage> {
    let mut prefix = [0u8; FRAME_PREFIX_LEN];
    read_exact(recv, &mut prefix).await?;
    // Validated before allocating, so a peer cannot ask for a huge buffer.
    let len = ControlMessage::body_len(prefix)?;
    let mut body = vec![0u8; len];
    read_exact(recv, &mut body).await?;
    Ok(ControlMessage::decode_body(&body)?)
}

async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) -> Result<()> {
    recv.read_exact(buf).await.map_err(|e| match e {
        ReadExactError::ReadError(ReadError::ConnectionLost(c)) => NetError::from_connection(c),
        _ => NetError::ControlClosed,
    })
}
