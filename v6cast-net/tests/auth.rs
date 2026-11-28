//! Group-token authentication and close-code behaviour, seen from outside.
//!
//! Well-behaved peers use the public `Client`; hostile ones are a raw QUIC
//! connection that writes bytes by hand on the first stream.

mod common;

use std::time::{Duration, Instant};

use common::*;
use v6cast_net::{close_code, NetError, TransportSettings};
use v6cast_protocol::control::{MAX_CONTROL_FRAME, MAX_PEER_NAME_LEN};
use v6cast_protocol::{
    ControlMessage, FrameRate, Hello, Preset, PresetRequest, ProtocolError, CONTROL_VERSION,
};

fn hello(token: &[u8], version: u16, name: &str) -> Vec<u8> {
    encode(&ControlMessage::Hello(Hello {
        protocol_version: version,
        token: token.to_vec(),
        peer_name: name.into(),
    }))
}

fn fast_auth() -> TransportSettings {
    TransportSettings {
        auth_timeout: Duration::from_millis(250),
        ..TransportSettings::default()
    }
}

#[tokio::test]
async fn correct_token_authenticates_and_both_sides_agree_on_the_session() {
    if !ipv6_or_skip() {
        return;
    }
    let mut server = server();
    let addr = server.local_addr().unwrap();
    assert!(addr.ip().is_loopback());
    let viewer = within("connect", client_named(TOKEN, "ana").connect(addr))
        .await
        .expect("authenticated");
    let streamer = within("accept", server.accept()).await.expect("session");
    assert_eq!(viewer.session_id(), streamer.session_id());
    assert_eq!(viewer.peer_name(), "streamer");
    assert_eq!(streamer.peer_name(), "ana");
    // Both ends see each other on the IPv6 loopback address, not an IPv4-mapped one.
    assert!(streamer.remote_address().is_ipv6() && streamer.remote_address().ip().is_loopback());
    assert_eq!(viewer.remote_address(), addr.into());
}

#[tokio::test]
async fn wrong_token_is_closed_at_once_and_never_surfaces_as_a_session() {
    if !ipv6_or_skip() {
        return;
    }
    let mut server = server(); // default 5 s authentication timeout
    let addr = server.local_addr().unwrap();

    let started = Instant::now();
    let result = within("wrong-token connect", client("wrong horse").connect(addr)).await;
    assert!(matches!(result, Err(NetError::AuthRejected)), "{result:?}");
    // Closed by the rejection, not by the timeout that is 5 s away.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );

    // The next session the server hands out is the good one: the rejected peer
    // never reached the application.
    let good = within("good connect", client_named(TOKEN, "good").connect(addr))
        .await
        .unwrap();
    let accepted = within("accept", server.accept()).await.unwrap();
    assert_eq!(accepted.peer_name(), "good");
    assert_eq!(accepted.session_id(), good.session_id());
}

#[tokio::test]
async fn near_miss_tokens_are_all_rejected() {
    if !ipv6_or_skip() {
        return;
    }
    let server = server();
    let addr = server.local_addr().unwrap();
    let real = TOKEN.as_bytes();
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("prefix", real[..real.len() - 1].to_vec()),
        ("one char more", [real, b"!"].concat()),
        ("trailing NUL", [real, &[0]].concat()),
        ("padded with zeros", [real, &[0; 20]].concat()),
        ("longest possible", vec![b'f'; 255]),
        ("reversed", real.iter().rev().copied().collect()),
        ("first byte differs", [b"F", &real[1..]].concat()),
        (
            "last byte differs",
            [&real[..real.len() - 1], b"Y"].concat(),
        ),
    ];
    let mut upper = real.to_vec();
    upper.make_ascii_uppercase();
    cases.push(("upper case", upper));

    for (name, token) in cases {
        let peer = RawPeer::connect(addr).await;
        let _streams = peer
            .send_on_new_stream(&hello(&token, CONTROL_VERSION, "x"))
            .await;
        assert_eq!(
            peer.close_code().await,
            u64::from(close_code::AUTH_FAILED),
            "{name}"
        );
    }
}

#[tokio::test]
async fn token_is_checked_before_the_version_so_strangers_learn_nothing() {
    if !ipv6_or_skip() {
        return;
    }
    let server = server();
    let addr = server.local_addr().unwrap();

    let stranger = RawPeer::connect(addr).await;
    let _s1 = stranger
        .send_on_new_stream(&hello(b"nope", CONTROL_VERSION + 7, "x"))
        .await;
    assert_eq!(
        stranger.close_code().await,
        u64::from(close_code::AUTH_FAILED)
    );

    let member = RawPeer::connect(addr).await;
    let _s2 = member
        .send_on_new_stream(&hello(TOKEN.as_bytes(), CONTROL_VERSION + 7, "x"))
        .await;
    assert_eq!(
        member.close_code().await,
        u64::from(close_code::VERSION_MISMATCH)
    );
}

#[tokio::test]
async fn silent_peer_is_closed_after_the_authentication_timeout() {
    if !ipv6_or_skip() {
        return;
    }
    let server = server_with(fast_auth());
    let peer = RawPeer::connect(server.local_addr().unwrap()).await;
    let started = Instant::now();
    assert_eq!(peer.close_code().await, u64::from(close_code::AUTH_TIMEOUT));
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn peer_that_stalls_in_the_middle_of_hello_is_closed_too() {
    if !ipv6_or_skip() {
        return;
    }
    let server = server_with(fast_auth());
    let frame = hello(TOKEN.as_bytes(), CONTROL_VERSION, "slow");
    // Every prefix of a valid Hello, cut short and then left hanging.
    for cut in [1, 2, 3, 4, 5, frame.len() - 1] {
        let peer = RawPeer::connect(server.local_addr().unwrap()).await;
        let _streams = peer.send_on_new_stream(&frame[..cut]).await;
        assert_eq!(
            peer.close_code().await,
            u64::from(close_code::AUTH_TIMEOUT),
            "cut at {cut}"
        );
    }
}

#[tokio::test]
async fn malformed_first_frames_are_protocol_violations() {
    if !ipv6_or_skip() {
        return;
    }
    let server = server();
    let addr = server.local_addr().unwrap();
    let preset = encode(&ControlMessage::PresetRequest(PresetRequest {
        preset: Preset::P720,
        frame_rate: FrameRate::Fps30,
    }));
    let too_long_name = hello(
        TOKEN.as_bytes(),
        CONTROL_VERSION,
        &"n".repeat(MAX_PEER_NAME_LEN),
    );
    let mut bad_utf8 = hello(TOKEN.as_bytes(), CONTROL_VERSION, "ab");
    let last = bad_utf8.len() - 1;
    bad_utf8[last - 1] = 0xff;
    bad_utf8[last] = 0xfe;
    let mut name_65 = too_long_name.clone();
    // Same frame, but claim a 65-byte name: still within the frame, past the limit.
    let name_len_pos = name_65.len() - MAX_PEER_NAME_LEN - 1;
    name_65[name_len_pos] = (MAX_PEER_NAME_LEN + 1) as u8;
    name_65.push(b'n');
    let new_len = (name_65.len() - 4) as u32;
    name_65[..4].copy_from_slice(&new_len.to_be_bytes());

    // The two name cases must really fail on the name, not on a framing slip.
    for bytes in [&bad_utf8, &name_65] {
        assert!(
            matches!(
                ControlMessage::decode_frame(bytes),
                Err(ProtocolError::InvalidField {
                    field: "peer_name",
                    ..
                })
            ),
            "{:?}",
            ControlMessage::decode_frame(bytes)
        );
    }

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("control message other than Hello", preset),
        ("unknown message type", vec![0, 0, 0, 3, 0xee, 1, 2]),
        ("zero-length frame", vec![0, 0, 0, 0]),
        (
            "length above the limit",
            ((MAX_CONTROL_FRAME as u32 + 1).to_be_bytes()).to_vec(),
        ),
        ("length of 4 GiB", vec![0xff, 0xff, 0xff, 0xff]),
        ("Hello body cut short", vec![0, 0, 0, 3, 0x01, 0x00, 0x01]),
        ("peer name with invalid UTF-8", bad_utf8),
        ("peer name over the limit", name_65),
    ];
    for (name, bytes) in cases {
        let peer = RawPeer::connect(addr).await;
        let _streams = peer.send_on_new_stream(&bytes).await;
        assert_eq!(
            peer.close_code().await,
            u64::from(close_code::PROTOCOL_VIOLATION),
            "{name}"
        );
    }
}

#[tokio::test]
async fn closing_the_stream_before_any_hello_counts_as_a_missing_token() {
    if !ipv6_or_skip() {
        return;
    }
    let server = server();
    let peer = RawPeer::connect(server.local_addr().unwrap()).await;
    let (mut send, _recv) = peer.send_on_new_stream(&[]).await;
    send.finish().unwrap();
    assert_eq!(peer.close_code().await, u64::from(close_code::AUTH_FAILED));
}

#[tokio::test]
async fn only_one_bidirectional_stream_and_no_unidirectional_streams() {
    if !ipv6_or_skip() {
        return;
    }
    let mut server = server();
    let peer = RawPeer::connect(server.local_addr().unwrap()).await;
    let (_send, mut recv) = peer
        .send_on_new_stream(&hello(TOKEN.as_bytes(), CONTROL_VERSION, "raw"))
        .await;
    assert!(matches!(
        read_control(&mut recv).await,
        ControlMessage::HelloAck(_)
    ));
    let _session = within("accept", server.accept()).await.unwrap();

    // The stream limits keep a peer from opening anything else: both calls wait
    // for credit that the server never grants.
    let second = tokio::time::timeout(Duration::from_millis(300), peer.connection.open_bi()).await;
    assert!(second.is_err(), "a second bidirectional stream was granted");
    let uni = tokio::time::timeout(Duration::from_millis(300), peer.connection.open_uni()).await;
    assert!(uni.is_err(), "a unidirectional stream was granted");
}

#[tokio::test]
async fn failed_attempts_do_not_stop_good_peers_from_connecting() {
    if !ipv6_or_skip() {
        return;
    }
    let mut server = server_with(fast_auth());
    let addr = server.local_addr().unwrap();

    // A burst of intruders, some silent, some with wrong tokens, at the same time.
    let mut attempts = Vec::new();
    for i in 0..24 {
        attempts.push(tokio::spawn(async move {
            if i % 3 == 0 {
                let peer = RawPeer::connect(addr).await;
                peer.close_code().await == u64::from(close_code::AUTH_TIMEOUT)
            } else {
                matches!(
                    client(&format!("guess {i}")).connect(addr).await,
                    Err(NetError::AuthRejected)
                )
            }
        }));
    }
    for attempt in attempts {
        assert!(
            within("intruder", attempt).await.unwrap(),
            "an intruder got through"
        );
    }

    let viewer = within("good connect", client_named(TOKEN, "ana").connect(addr))
        .await
        .unwrap();
    let streamer = within("accept", server.accept()).await.unwrap();
    assert_eq!(streamer.peer_name(), "ana");
    drop(viewer);
}

#[tokio::test]
async fn orderly_close_reports_code_zero_to_the_peer() {
    if !ipv6_or_skip() {
        return;
    }
    let (server, _c, viewer, streamer) = {
        let mut server = server();
        let client = client(TOKEN);
        let viewer = within("connect", client.connect(server.local_addr().unwrap()))
            .await
            .unwrap();
        let streamer = within("accept", server.accept()).await.unwrap();
        (server, client, viewer, streamer)
    };
    viewer.close();
    let reason = within("streamer sees the close", streamer.closed()).await;
    assert!(
        matches!(reason, NetError::ClosedByPeer { code } if code == u64::from(close_code::NORMAL)),
        "{reason:?}"
    );
    // Using a closed session fails instead of hanging or panicking.
    assert!(streamer
        .send_datagram(bytes::Bytes::from_static(b"x"))
        .is_err());
    assert!(within("recv_control", streamer.recv_control())
        .await
        .is_err());
    server.close();
}

#[tokio::test]
async fn server_close_ends_every_session_with_code_zero() {
    if !ipv6_or_skip() {
        return;
    }
    let mut server = server();
    let addr = server.local_addr().unwrap();
    let mut viewers = Vec::new();
    let mut clients = Vec::new();
    for _ in 0..3 {
        let c = client(TOKEN);
        viewers.push(within("connect", c.connect(addr)).await.unwrap());
        clients.push(c);
        let _ = within("accept", server.accept()).await.unwrap();
    }
    server.close();
    for viewer in &viewers {
        let reason = within("viewer sees shutdown", viewer.closed()).await;
        assert!(
            matches!(reason, NetError::ClosedByPeer { code: 0 }),
            "{reason:?}"
        );
    }
}

#[tokio::test]
async fn dropping_a_viewer_ends_the_streamers_session() {
    if !ipv6_or_skip() {
        return;
    }
    let mut server = server();
    let c = client(TOKEN);
    let viewer = within("connect", c.connect(server.local_addr().unwrap()))
        .await
        .unwrap();
    let streamer = within("accept", server.accept()).await.unwrap();
    drop(viewer);
    drop(c);
    // No explicit close: letting go of every handle must still end the connection.
    let reason = within("streamer notices", streamer.closed()).await;
    assert!(
        matches!(reason, NetError::ClosedByPeer { .. }),
        "{reason:?}"
    );
    assert!(streamer
        .send_datagram(bytes::Bytes::from_static(b"x"))
        .is_err());
}
