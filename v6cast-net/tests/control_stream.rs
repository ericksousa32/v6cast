//! The reliable control stream: ordering, hostile input after authentication, and
//! behaviour when the peer goes away.

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use v6cast_net::{NetError, Session};
use v6cast_protocol::control::MAX_CONTROL_FRAME;
use v6cast_protocol::{
    Codec, ControlMessage, FrameRate, Hello, KeyframeRequest, Preset, PresetRequest, ProtocolError,
    StreamConfig, TelemetryReport, CONTROL_VERSION,
};

async fn pair() -> (
    v6cast_net::Server,
    v6cast_net::Client,
    Arc<Session>,
    Arc<Session>,
) {
    let mut server = server();
    let client = client(TOKEN);
    let viewer = within("connect", client.connect(server.local_addr().unwrap()))
        .await
        .unwrap();
    let streamer = within("accept", server.accept()).await.unwrap();
    (server, client, Arc::new(streamer), Arc::new(viewer))
}

/// A raw peer that completed the handshake, plus the server's session for it.
async fn authenticated_raw_peer() -> (
    v6cast_net::Server,
    RawPeer,
    quinn::SendStream,
    quinn::RecvStream,
    Session,
) {
    let mut server = server();
    let peer = RawPeer::connect(server.local_addr().unwrap()).await;
    let hello = encode(&ControlMessage::Hello(Hello {
        protocol_version: CONTROL_VERSION,
        token: TOKEN.into(),
        peer_name: "raw".into(),
    }));
    let (send, mut recv) = peer.send_on_new_stream(&hello).await;
    assert!(matches!(
        read_control(&mut recv).await,
        ControlMessage::HelloAck(_)
    ));
    let session = within("accept", server.accept()).await.unwrap();
    (server, peer, send, recv, session)
}

fn mixed(i: u32) -> ControlMessage {
    match i % 5 {
        0 => ControlMessage::PresetRequest(PresetRequest {
            preset: Preset::P1440,
            frame_rate: FrameRate::Fps60,
        }),
        1 => ControlMessage::KeyframeRequest(KeyframeRequest {
            last_complete_frame_id: Some(i),
        }),
        2 => ControlMessage::TelemetryReport(TelemetryReport {
            rtt_us: i,
            jitter_us: i / 2,
            loss_ppm: i % 1_000_000,
            bandwidth_bps: u64::from(i) * 1_000,
        }),
        3 => ControlMessage::StreamConfig(StreamConfig {
            preset: Preset::P1080,
            frame_rate: FrameRate::Fps30,
            width: 1 + (i % 2560) as u16,
            height: 1 + (i % 1440) as u16,
            codec: Codec::H264,
        }),
        _ => ControlMessage::KeyframeRequest(KeyframeRequest {
            last_complete_frame_id: None,
        }),
    }
}

#[tokio::test]
async fn control_messages_arrive_complete_and_in_order_in_both_directions() {
    if !ipv6_or_skip() {
        return;
    }
    let (_server, _client, streamer, viewer) = pair().await;
    const N: u32 = 300;
    let send = |s: Arc<Session>| async move {
        for i in 0..N {
            s.send_control(&mixed(i)).await.unwrap();
        }
    };
    let receive = |s: Arc<Session>| async move {
        for i in 0..N {
            assert_eq!(
                within("control", s.recv_control()).await.unwrap(),
                mixed(i),
                "message {i}"
            );
        }
    };
    tokio::join!(
        send(streamer.clone()),
        send(viewer.clone()),
        receive(streamer.clone()),
        receive(viewer.clone()),
    );
}

#[tokio::test]
async fn datagrams_up_to_the_path_limit_pass_and_larger_ones_are_refused() {
    if !ipv6_or_skip() {
        return;
    }
    let (_server, _client, streamer, viewer) = pair().await;
    let max = streamer.max_datagram_size().expect("datagrams enabled");
    assert!(max >= v6cast_protocol::IPV6_SAFE_DATAGRAM, "{max}");

    streamer.send_datagram(Bytes::from(vec![9u8; max])).unwrap();
    assert_eq!(
        within("datagram", viewer.recv_datagram())
            .await
            .unwrap()
            .len(),
        max
    );

    // Far above any path MTU (it can still grow while path MTU discovery runs, so
    // "one byte more than the current limit" would be a flaky choice).
    let too_big = streamer.send_datagram(Bytes::from(vec![9u8; 100_000]));
    assert!(
        matches!(too_big, Err(NetError::SendDatagram(_))),
        "{too_big:?}"
    );
    // The session is still healthy afterwards.
    streamer
        .send_datagram(Bytes::from_static(b"still here"))
        .unwrap();
    assert_eq!(
        &within("datagram", viewer.recv_datagram()).await.unwrap()[..],
        b"still here"
    );
}

#[tokio::test]
async fn hostile_frames_after_authentication_are_errors_not_panics_or_big_allocations() {
    if !ipv6_or_skip() {
        return;
    }
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "length above the limit",
            ((MAX_CONTROL_FRAME as u32 + 1).to_be_bytes()).to_vec(),
        ),
        ("length of 4 GiB", vec![0xff; 4]),
        ("zero length", vec![0, 0, 0, 0]),
        ("unknown type", vec![0, 0, 0, 2, 0xee, 0]),
        ("trailing bytes", vec![0, 0, 0, 5, 0x12, 0, 9, 9, 9]),
    ];
    for (name, bytes) in cases {
        let (_server, _peer, mut send, _recv, session) = authenticated_raw_peer().await;
        send.write_all(&bytes).await.unwrap();
        let result = within(name, session.recv_control()).await;
        assert!(
            matches!(result, Err(NetError::Protocol(_))),
            "{name}: {result:?}"
        );
        if name == "length above the limit" {
            assert!(
                matches!(
                    result,
                    Err(NetError::Protocol(ProtocolError::BadFrameLength { .. }))
                ),
                "{result:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_peer_that_ends_its_control_stream_is_reported_not_waited_for() {
    if !ipv6_or_skip() {
        return;
    }
    let (_server, _peer, mut send, _recv, session) = authenticated_raw_peer().await;
    send.finish().unwrap();
    let result = within("recv_control", session.recv_control()).await;
    assert!(matches!(result, Err(NetError::ControlClosed)), "{result:?}");
}

#[tokio::test]
async fn a_pending_recv_control_ends_when_the_peer_closes() {
    if !ipv6_or_skip() {
        return;
    }
    let (_server, _client, streamer, viewer) = pair().await;
    let waiting = {
        let streamer = streamer.clone();
        tokio::spawn(async move { streamer.recv_control().await })
    };
    tokio::task::yield_now().await;
    viewer.close();
    let result = within("recv_control", waiting).await.unwrap();
    assert!(
        matches!(result, Err(NetError::ClosedByPeer { code: 0 })),
        "{result:?}"
    );
}

/// Dropping `Session::recv_control` half-way through a frame, as happens inside
/// `tokio::select!` or `tokio::time::timeout`, must not lose the bytes already
/// read: the next call has to finish the same frame. Control frames are small,
/// so honest peers rarely split one, but a hostile or just unlucky one can.
#[tokio::test]
async fn recv_control_survives_being_cancelled_in_the_middle_of_a_frame() {
    if !ipv6_or_skip() {
        return;
    }
    let (_server, _peer, mut send, _recv, session) = authenticated_raw_peer().await;
    let frame = encode(&mixed(0));

    send.write_all(&frame[..2]).await.unwrap(); // half of the length prefix
    let cancelled = tokio::time::timeout(Duration::from_millis(100), session.recv_control()).await;
    assert!(
        cancelled.is_err(),
        "the frame is incomplete, the call must still be waiting"
    );

    send.write_all(&frame[2..]).await.unwrap();
    let message = within("recv_control", session.recv_control()).await;
    assert_eq!(message.unwrap(), mixed(0));
}
