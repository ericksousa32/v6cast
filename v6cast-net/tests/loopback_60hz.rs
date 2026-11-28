//! Two peers authenticate and exchange synthetic media at 60 Hz over IPv6 loopback.
//!
//! The stream is a real-time one: frames are paced by the clock, not pushed in a
//! burst, so the test exercises the same send cadence a 60 fps capture produces.
//! Latency bounds are deliberately loose (a loaded CI runner must pass); what they
//! catch is a stall of the data path, not a regression of a few milliseconds.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use tokio::time::{interval, MissedTickBehavior};
use v6cast_net::{ReceiverTelemetry, Session};
use v6cast_protocol::{
    ControlMessage, Fragmenter, MediaFlags, MediaKind, Push, Reassembler, IPV6_SAFE_DATAGRAM,
};

const FRAMES: u64 = 90; // 1.5 s at 60 Hz
const PERIOD: Duration = Duration::from_micros(16_667);

fn frame_data(i: u64) -> Vec<u8> {
    let len = if i.is_multiple_of(30) {
        24_000
    } else {
        2_000 + (i as usize * 37) % 1_500
    };
    (0..len).map(|k| (i as usize * 31 + k) as u8).collect()
}

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
}

/// Video from the streamer, fragmented to the IPv6-safe size, one frame per tick.
async fn stream_video(session: Arc<Session>, start: Instant) {
    let max = session.max_datagram_size().expect("datagrams enabled");
    assert!(
        max >= IPV6_SAFE_DATAGRAM,
        "path accepts only {max} byte datagrams"
    );
    let mut fragmenter = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let mut ticker = interval(PERIOD);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
    for i in 0..FRAMES {
        ticker.tick().await;
        let flags = if i.is_multiple_of(30) {
            MediaFlags::keyframe()
        } else {
            MediaFlags::default()
        };
        let ts = start.elapsed().as_micros() as u64;
        for d in fragmenter.fragment(&frame_data(i), flags, ts).unwrap() {
            session
                .send_datagram(Bytes::from(d))
                .expect("queue datagram");
        }
    }
}

/// 8-byte probes (sequence, send time in microseconds) from the viewer at 60 Hz.
async fn send_probes(session: Arc<Session>, start: Instant) {
    let mut ticker = interval(PERIOD);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
    for seq in 0..FRAMES as u32 {
        ticker.tick().await;
        let mut probe = Vec::with_capacity(12);
        probe.extend_from_slice(&seq.to_be_bytes());
        probe.extend_from_slice(&(start.elapsed().as_micros() as u64).to_be_bytes());
        session
            .send_datagram(Bytes::from(probe))
            .expect("queue probe");
    }
}

#[tokio::test]
async fn synthetic_60hz_stream_both_ways_over_ipv6_loopback() {
    if !ipv6_or_skip() {
        return;
    }
    let mut server = server();
    let addr = server.local_addr().unwrap();
    assert!(addr.ip().is_loopback());
    let client = client(TOKEN);
    let viewer = Arc::new(within("connect", client.connect(addr)).await.unwrap());
    let streamer = Arc::new(within("accept", server.accept()).await.unwrap());

    let start = Instant::now();
    let video = tokio::spawn(stream_video(streamer.clone(), start));
    let probes = tokio::spawn(send_probes(viewer.clone(), start));

    // Streamer side: collect the probes from the viewer.
    let probe_rx = {
        let streamer = streamer.clone();
        tokio::spawn(async move {
            let mut latencies = Vec::new();
            for expected in 0..FRAMES as u32 {
                let d = within("probe", streamer.recv_datagram()).await.unwrap();
                assert_eq!(d.len(), 12);
                let seq = u32::from_be_bytes(d[..4].try_into().unwrap());
                let sent = u64::from_be_bytes(d[4..].try_into().unwrap());
                assert_eq!(seq, expected, "probes arrived out of order or were lost");
                latencies.push(start.elapsed() - Duration::from_micros(sent));
            }
            latencies
        })
    };

    // Viewer side: reassemble the video and measure.
    let mut telemetry = ReceiverTelemetry::new();
    let mut reassembler = Reassembler::default();
    let mut frame_latencies = Vec::new();
    let mut next_frame = 0u64;
    while next_frame < FRAMES {
        let d = within("video datagram", viewer.recv_datagram())
            .await
            .unwrap();
        let now = Instant::now();
        telemetry.on_datagram(&d, now);
        if let Push::Complete(frame) = reassembler.push_datagram(&d, now) {
            telemetry.on_frame(&frame, now);
            assert_eq!(u64::from(frame.frame_id), next_frame, "frames out of order");
            assert_eq!(
                frame.data,
                frame_data(next_frame),
                "frame {next_frame} corrupted"
            );
            assert_eq!(frame.flags.is_keyframe(), next_frame.is_multiple_of(30));
            frame_latencies.push(
                start
                    .elapsed()
                    .saturating_sub(Duration::from_micros(frame.timestamp_us)),
            );
            next_frame += 1;
        }
    }
    let elapsed = start.elapsed();
    video.await.unwrap();
    probes.await.unwrap();
    let mut probe_latencies = probe_rx.await.unwrap();

    // The stream really ran at the 60 Hz cadence, not as a burst.
    let ideal = PERIOD * (FRAMES as u32 - 1);
    assert!(
        elapsed >= ideal.mul_f64(0.9),
        "finished in {elapsed:?}, faster than 60 Hz allows"
    );

    // Nothing lost or reordered on the loopback path.
    let stats = reassembler.stats();
    assert_eq!(stats.frames_completed, FRAMES);
    assert_eq!(
        stats.frames_dropped + stats.fragments_late + stats.fragments_invalid,
        0
    );
    let report = telemetry.report(viewer.connection().rtt(), Instant::now());
    assert_eq!(report.loss_ppm, 0, "{report:?}");
    assert!(report.bandwidth_bps > 0, "{report:?}");
    assert!(report.rtt_us > 0 && report.rtt_us < 250_000, "{report:?}");

    // Latency stays flat: the typical frame is fast and the slowest is not far off.
    for (name, latencies) in [
        ("video", &mut frame_latencies),
        ("probe", &mut probe_latencies),
    ] {
        latencies.sort();
        let (median, p95, worst) = (
            percentile(latencies, 50),
            percentile(latencies, 95),
            *latencies.last().unwrap(),
        );
        eprintln!("{name} latency: median {median:?}, p95 {p95:?}, worst {worst:?}");
        assert!(
            median < Duration::from_millis(50),
            "{name} median {median:?}"
        );
        assert!(p95 < Duration::from_millis(150), "{name} p95 {p95:?}");
        assert!(worst < Duration::from_millis(500), "{name} worst {worst:?}");
    }

    // The receiver's report goes back on the reliable stream and arrives intact.
    let message = ControlMessage::TelemetryReport(report);
    viewer.send_control(&message).await.unwrap();
    assert_eq!(
        within("report", streamer.recv_control()).await.unwrap(),
        message
    );
}
