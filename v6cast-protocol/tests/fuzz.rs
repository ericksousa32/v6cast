//! Fuzz-style tests: the decoders and the reassembler get hostile bytes and must
//! neither panic nor grow without bound.
//!
//! The generator is a fixed-seed xorshift, so a failure replays exactly. Raise
//! `V6CAST_FUZZ_ROUNDS` to run longer locally (default: a few seconds in debug).

use std::time::{Duration, Instant};

use v6cast_protocol::control::{FRAME_PREFIX_LEN, MAX_CONTROL_FRAME};
use v6cast_protocol::media::MAX_FRAME_LEN;
use v6cast_protocol::{
    decode_datagram, fragment::fragment_span, ControlMessage, FrameRate, Hello, HelloAck,
    KeyframeRequest, MediaFlags, MediaHeader, MediaKind, Preset, PresetRequest, Push, Reassembler,
    ReassemblyConfig, StreamConfig, TelemetryReport, CONTROL_VERSION, MEDIA_HEADER_LEN,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn rounds(default: u64) -> u64 {
    std::env::var("V6CAST_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn sample_messages() -> Vec<ControlMessage> {
    vec![
        ControlMessage::Hello(Hello {
            protocol_version: CONTROL_VERSION,
            token: b"friends only".to_vec(),
            peer_name: "ana".into(),
        }),
        ControlMessage::Hello(Hello {
            protocol_version: CONTROL_VERSION,
            token: vec![0xAB; 255],
            peer_name: "x".repeat(64),
        }),
        ControlMessage::HelloAck(HelloAck {
            protocol_version: CONTROL_VERSION,
            session_id: u64::MAX,
            peer_name: "streamer".into(),
        }),
        ControlMessage::StreamConfig(StreamConfig {
            preset: Preset::Native,
            frame_rate: FrameRate::Fps60,
            width: 2560,
            height: 1440,
            codec: v6cast_protocol::Codec::H264,
        }),
        ControlMessage::PresetRequest(PresetRequest {
            preset: Preset::P720,
            frame_rate: FrameRate::Fps30,
        }),
        ControlMessage::KeyframeRequest(KeyframeRequest {
            last_complete_frame_id: Some(7),
        }),
        ControlMessage::KeyframeRequest(KeyframeRequest {
            last_complete_frame_id: None,
        }),
        ControlMessage::TelemetryReport(TelemetryReport {
            rtt_us: 1200,
            jitter_us: 80,
            loss_ppm: 1_000_000,
            bandwidth_bps: 50_000_000,
        }),
    ]
}

fn valid_datagram(rng: &mut Rng) -> Vec<u8> {
    let count = 1 + rng.below(6) as u16;
    let frame_len = u32::from(count) * 100 + rng.below(100) as u32;
    let index = rng.below(u64::from(count)) as u16;
    let (_, len) = fragment_span(frame_len, count, index).unwrap();
    MediaHeader {
        kind: if rng.below(2) == 0 {
            MediaKind::Video
        } else {
            MediaKind::Audio
        },
        flags: MediaFlags(rng.next() as u8),
        seq: rng.next() as u32,
        frame_id: rng.next() as u32,
        fragment_index: index,
        fragment_count: count,
        frame_len,
        timestamp_us: rng.next(),
    }
    .encode_datagram(&rng.bytes(len))
}

/// Feeds the same bytes to every decoder. Success is "did not panic".
fn poke_all_decoders(bytes: &[u8]) {
    let _ = decode_datagram(bytes);
    let _ = ControlMessage::decode_body(bytes);
    let _ = ControlMessage::decode_frame(bytes);
    if let Some(prefix) = bytes.get(..FRAME_PREFIX_LEN) {
        let _ = ControlMessage::body_len(prefix.try_into().unwrap());
    }
}

#[test]
fn random_bytes_never_panic_any_decoder() {
    let mut rng = Rng(0x00C0_FFEE);
    for _ in 0..rounds(300_000) {
        // Lengths cluster around the header sizes, where off-by-one bugs live.
        let len = match rng.below(4) {
            0 => rng.below(8) as usize,
            1 => MEDIA_HEADER_LEN - 2 + rng.below(5) as usize,
            2 => rng.below(64) as usize,
            _ => rng.below(300) as usize,
        };
        poke_all_decoders(&rng.bytes(len));
    }
}

#[test]
fn every_truncation_and_single_byte_change_of_valid_input_is_handled() {
    let mut inputs: Vec<Vec<u8>> = sample_messages()
        .iter()
        .map(|m| m.encode_frame().unwrap())
        .collect();
    let mut rng = Rng(7);
    inputs.extend((0..16).map(|_| valid_datagram(&mut rng)));

    for input in &inputs {
        for cut in 0..=input.len() {
            poke_all_decoders(&input[..cut]);
        }
        for pos in 0..input.len() {
            for value in [0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff] {
                let mut copy = input.clone();
                copy[pos] = value;
                poke_all_decoders(&copy);
            }
        }
        let mut longer = input.clone();
        longer.extend_from_slice(&[0xAA; 9]);
        poke_all_decoders(&longer);
    }
}

#[test]
fn accepted_control_bodies_are_canonical() {
    // If a mutated body still decodes, encoding it again must give the same
    // message back: the decoder accepts nothing it could not have produced.
    let mut rng = Rng(99);
    let frames: Vec<Vec<u8>> = sample_messages()
        .iter()
        .map(|m| m.encode_frame().unwrap())
        .collect();
    let mut accepted = 0u32;
    for _ in 0..rounds(150_000) {
        let frame = &frames[rng.below(frames.len() as u64) as usize];
        let mut body = frame[FRAME_PREFIX_LEN..].to_vec();
        for _ in 0..1 + rng.below(3) {
            let pos = rng.below(body.len() as u64) as usize;
            body[pos] = rng.next() as u8;
        }
        if let Ok(msg) = ControlMessage::decode_body(&body) {
            accepted += 1;
            let again = msg.encode_frame().expect("accepted message must re-encode");
            assert_eq!(&again[FRAME_PREFIX_LEN..], &body[..], "{msg:?}");
        }
    }
    assert!(
        accepted > 1000,
        "mutations were too destructive: {accepted}"
    );
}

#[test]
fn accepted_datagrams_satisfy_every_header_invariant() {
    let mut rng = Rng(5);
    let mut accepted = 0u32;
    for _ in 0..rounds(200_000) {
        let mut d = valid_datagram(&mut rng);
        for _ in 0..rng.below(4) {
            let pos = rng.below(d.len() as u64) as usize;
            d[pos] = rng.next() as u8;
        }
        if rng.below(8) == 0 {
            d.truncate(rng.below(d.len() as u64 + 1) as usize);
        }
        if let Ok((h, payload)) = decode_datagram(&d) {
            accepted += 1;
            assert!(h.fragment_count >= 1 && h.fragment_index < h.fragment_count);
            assert!((1..=MAX_FRAME_LEN).contains(&h.frame_len));
            assert!(u32::from(h.fragment_count) <= h.frame_len);
            assert!(!payload.is_empty() && payload.len() <= h.frame_len as usize);
            assert_eq!(payload.len(), d.len() - MEDIA_HEADER_LEN);
        }
    }
    assert!(
        accepted > 1000,
        "mutations were too destructive: {accepted}"
    );
}

#[test]
fn declared_control_lengths_are_checked_before_any_allocation() {
    for (len, ok) in [
        (0u32, false),
        (1, true),
        (MAX_CONTROL_FRAME as u32, true),
        (MAX_CONTROL_FRAME as u32 + 1, false),
        (0x7fff_ffff, false),
        (u32::MAX, false),
    ] {
        let r = ControlMessage::body_len(len.to_be_bytes());
        assert_eq!(r.is_ok(), ok, "{len}");
        // decode_frame sees only the prefix, so it must wait or fail, never read ahead.
        let r = ControlMessage::decode_frame(&len.to_be_bytes());
        match (ok, r) {
            (true, Ok(None)) | (false, Err(_)) => {}
            (_, other) => panic!("prefix {len}: unexpected {other:?}"),
        }
    }
}

#[test]
fn streaming_decode_matches_message_boundaries() {
    let messages = sample_messages();
    let wire: Vec<u8> = messages
        .iter()
        .flat_map(|m| m.encode_frame().unwrap())
        .collect();
    // Feed the stream in chunks of every size, as a transport would.
    for chunk in 1..=17 {
        let mut buf = Vec::new();
        let mut decoded = Vec::new();
        for piece in wire.chunks(chunk) {
            buf.extend_from_slice(piece);
            while let Some((msg, used)) = ControlMessage::decode_frame(&buf).unwrap() {
                decoded.push(msg);
                buf.drain(..used);
            }
        }
        assert!(buf.is_empty(), "chunk {chunk}");
        assert_eq!(decoded, messages, "chunk {chunk}");
    }
}

fn check_budget(r: &Reassembler, config: &ReassemblyConfig) {
    assert!(
        r.buffered_bytes() <= config.max_buffered_bytes,
        "buffered {} above budget {}",
        r.buffered_bytes(),
        config.max_buffered_bytes
    );
    assert!(r.partial_frames() <= config.max_partial_frames);
}

#[test]
fn reassembler_survives_random_datagrams_within_budget() {
    let config = ReassemblyConfig::default();
    let mut r = Reassembler::new(config);
    let mut rng = Rng(1234);
    let start = Instant::now();
    for i in 0..rounds(200_000) {
        let now = start + Duration::from_millis(i / 8);
        let d = if rng.below(2) == 0 {
            valid_datagram(&mut rng)
        } else {
            let len = rng.below(120) as usize;
            rng.bytes(len)
        };
        r.push_datagram(&d, now);
        check_budget(&r, &config);
    }
}

#[test]
fn well_formed_hostile_headers_cannot_exceed_the_memory_budget() {
    // Every datagram here passes header validation and has the exact payload
    // length for its slot, so only the reassembler's own limits stand in the way.
    // One 1 KiB datagram may ask for an 8 MiB frame.
    for config in [
        ReassemblyConfig::default(),
        ReassemblyConfig {
            max_partial_frames: 3,
            max_buffered_bytes: 10 * 1024 * 1024,
            max_frame_age: Duration::from_millis(100),
        },
        ReassemblyConfig {
            max_partial_frames: 64,
            max_buffered_bytes: 1024,
            max_frame_age: Duration::from_secs(60),
        },
    ] {
        let mut r = Reassembler::new(config);
        let mut rng = Rng(42);
        let start = Instant::now();
        for i in 0..rounds(2_000) {
            let frame_len = 1 + rng.below(u64::from(MAX_FRAME_LEN)) as u32;
            // Enough fragments that each payload is about one datagram long, so
            // the fragment is accepted and the full frame buffer gets allocated.
            let count = frame_len.div_ceil(1100) as u16 + rng.below(3) as u16;
            let index = rng.below(u64::from(count)) as u16;
            let header = MediaHeader {
                kind: MediaKind::Video,
                flags: MediaFlags::default(),
                seq: i as u32,
                // A small id space makes fragments of the same frame collide.
                frame_id: rng.below(40) as u32 + 1_000,
                fragment_index: index,
                fragment_count: count,
                frame_len,
                timestamp_us: 0,
            };
            let push = match fragment_span(frame_len, count, index) {
                Some((_, len)) => r.push(
                    &header,
                    &vec![0u8; len],
                    start + Duration::from_millis(i / 4),
                ),
                // The layout is impossible (offset past the end): must be refused.
                None => {
                    let push = r.push(&header, &[0u8; 8], start);
                    assert!(matches!(push, Push::Dropped(_)), "{header:?}");
                    push
                }
            };
            if let Push::Complete(frame) = push {
                assert_eq!(frame.data.len(), frame_len as usize);
            }
            check_budget(&r, &config);
        }
    }
}

#[test]
fn flooding_one_frame_id_with_conflicting_headers_keeps_one_partial() {
    let config = ReassemblyConfig::default();
    let mut r = Reassembler::new(config);
    let now = Instant::now();
    let mut rng = Rng(3);
    for _ in 0..rounds(20_000) {
        let count = 2 + rng.below(8) as u16;
        let frame_len = u32::from(count) * 50 + rng.below(50) as u32;
        let index = rng.below(u64::from(count)) as u16;
        let (_, len) = fragment_span(frame_len, count, index).unwrap();
        let header = MediaHeader {
            kind: MediaKind::Audio,
            flags: MediaFlags(rng.below(2) as u8),
            seq: 0,
            frame_id: 77,
            fragment_index: index,
            fragment_count: count,
            frame_len,
            timestamp_us: rng.below(2),
        };
        r.push(&header, &vec![1u8; len], now);
        assert!(
            r.partial_frames() <= 1,
            "same frame id, one partial at most"
        );
        check_budget(&r, &config);
    }
}
