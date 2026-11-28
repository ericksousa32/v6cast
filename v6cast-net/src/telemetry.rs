//! Network telemetry: RTT, loss, jitter and bandwidth.
//!
//! Receiver side ([`ReceiverTelemetry`]):
//! * loss from gaps in the media datagram sequence numbers
//!   ([`SequenceTracker`]), per media kind, tolerant to reordering and duplicates;
//! * interarrival jitter of frames (RFC 3550, section 6.4.1);
//! * receive bandwidth as an exponentially weighted moving average.
//!
//! The receiver sends these back to the sender in a
//! [`TelemetryReport`] together with the QUIC RTT estimate.
//!
//! Sender side ([`PathSnapshot`], [`PathLossMeter`]): the QUIC connection's own
//! RTT, congestion window and packet loss counters.

use std::time::{Duration, Instant};

use quinn::Connection;
use v6cast_protocol::fragment::Frame;
use v6cast_protocol::{decode_datagram, MediaKind, TelemetryReport};

const PPM: u64 = 1_000_000;

fn ppm(lost: u64, expected: u64) -> u32 {
    // lost <= expected, so the quotient never exceeds 1_000_000.
    (lost.min(expected) * PPM)
        .checked_div(expected)
        .unwrap_or(0) as u32
}

/// Loss over one measurement interval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LossSample {
    pub expected: u64,
    pub received: u64,
}

impl LossSample {
    pub fn lost(&self) -> u64 {
        self.expected.saturating_sub(self.received)
    }

    /// Loss in parts per million.
    pub fn loss_ppm(&self) -> u32 {
        ppm(self.lost(), self.expected)
    }
}

/// Counts received and missing datagram sequence numbers.
///
/// A sliding window of the last [`SequenceTracker::WINDOW`] sequence numbers
/// detects duplicates and credits datagrams that arrive out of order. A jump
/// larger than [`SequenceTracker::MAX_GAP`] is treated as a sender restart and
/// resets the tracker instead of being counted as loss.
#[derive(Debug, Clone)]
pub struct SequenceTracker {
    highest: Option<u32>,
    seen: [u64; Self::WINDOW / 64],
    interval: LossSample,
    total: LossSample,
    duplicates: u64,
    too_old: u64,
}

impl Default for SequenceTracker {
    fn default() -> Self {
        Self {
            highest: None,
            seen: [0; Self::WINDOW / 64],
            interval: LossSample::default(),
            total: LossSample::default(),
            duplicates: 0,
            too_old: 0,
        }
    }
}

impl SequenceTracker {
    /// Reordering window, in datagrams.
    pub const WINDOW: usize = 1024;
    /// Largest forward jump still counted as loss.
    pub const MAX_GAP: u32 = 1 << 16;

    pub fn new() -> Self {
        Self::default()
    }

    fn slot(seq: u32) -> (usize, u64) {
        let i = seq as usize % Self::WINDOW;
        (i / 64, 1u64 << (i % 64))
    }

    fn is_seen(&self, seq: u32) -> bool {
        let (word, bit) = Self::slot(seq);
        self.seen[word] & bit != 0
    }

    fn set_seen(&mut self, seq: u32, seen: bool) {
        let (word, bit) = Self::slot(seq);
        if seen {
            self.seen[word] |= bit;
        } else {
            self.seen[word] &= !bit;
        }
    }

    fn count(&mut self, expected: u64, received: u64) {
        for s in [&mut self.interval, &mut self.total] {
            s.expected += expected;
            s.received += received;
        }
    }

    fn restart(&mut self, seq: u32) {
        self.seen = [0; Self::WINDOW / 64];
        self.highest = Some(seq);
        self.set_seen(seq, true);
        self.count(1, 1);
    }

    /// Records one received sequence number.
    pub fn record(&mut self, seq: u32) {
        let Some(highest) = self.highest else {
            self.restart(seq);
            return;
        };
        let ahead = seq.wrapping_sub(highest);
        if ahead == 0 {
            self.duplicates += 1;
        } else if ahead < 0x8000_0000 {
            if ahead > Self::MAX_GAP {
                self.restart(seq);
                return;
            }
            // Sequence numbers skipped over are expected but not (yet) received.
            let cleared = (ahead as usize).min(Self::WINDOW);
            for k in 1..=cleared as u32 {
                self.set_seen(highest.wrapping_add(k), false);
            }
            self.set_seen(seq, true);
            self.highest = Some(seq);
            self.count(u64::from(ahead), 1);
        } else {
            let behind = highest.wrapping_sub(seq);
            if behind as usize >= Self::WINDOW {
                self.too_old += 1;
            } else if self.is_seen(seq) {
                self.duplicates += 1;
            } else {
                // Arrived out of order: already counted as expected.
                self.set_seen(seq, true);
                self.count(0, 1);
            }
        }
    }

    /// Loss since the previous call, then starts a new interval.
    pub fn take_interval(&mut self) -> LossSample {
        std::mem::take(&mut self.interval)
    }

    pub fn total(&self) -> LossSample {
        self.total
    }

    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    /// Datagrams that arrived after the reordering window had moved past them.
    pub fn too_old(&self) -> u64 {
        self.too_old
    }
}

/// Interarrival jitter (RFC 3550): smoothed variation of transit time.
#[derive(Debug, Clone, Default)]
pub struct JitterEstimator {
    last: Option<(Instant, u64)>,
    jitter_us: f64,
}

impl JitterEstimator {
    /// `sent_us` is the sender's capture timestamp, `arrival` the local receive time.
    pub fn record(&mut self, sent_us: u64, arrival: Instant) {
        if let Some((prev_arrival, prev_sent)) = self.last {
            if sent_us > prev_sent {
                let arrival_delta =
                    arrival.saturating_duration_since(prev_arrival).as_micros() as f64;
                let send_delta = (sent_us - prev_sent) as f64;
                let d = (arrival_delta - send_delta).abs();
                self.jitter_us += (d - self.jitter_us) / 16.0;
            }
        }
        self.last = Some((arrival, sent_us));
    }

    pub fn jitter(&self) -> Duration {
        Duration::from_micros(self.jitter_us as u64)
    }
}

/// Receive rate as an exponentially weighted moving average over short windows.
#[derive(Debug, Clone)]
pub struct BandwidthEstimator {
    window: Duration,
    alpha: f64,
    started: Option<Instant>,
    bytes: u64,
    rate_bps: Option<f64>,
}

impl Default for BandwidthEstimator {
    fn default() -> Self {
        Self::new(Duration::from_millis(250), 0.25)
    }
}

impl BandwidthEstimator {
    pub fn new(window: Duration, alpha: f64) -> Self {
        Self {
            window,
            alpha: alpha.clamp(0.0, 1.0),
            started: None,
            bytes: 0,
            rate_bps: None,
        }
    }

    pub fn record(&mut self, bytes: usize, now: Instant) {
        let started = *self.started.get_or_insert(now);
        let elapsed = now.saturating_duration_since(started);
        if elapsed >= self.window {
            self.close_window(elapsed);
            self.started = Some(now);
        }
        self.bytes += bytes as u64;
    }

    fn close_window(&mut self, elapsed: Duration) {
        let sample = self.bytes as f64 * 8.0 / elapsed.as_secs_f64();
        self.rate_bps = Some(match self.rate_bps {
            Some(rate) => self.alpha * sample + (1.0 - self.alpha) * rate,
            None => sample,
        });
        self.bytes = 0;
    }

    /// Current estimate in bits per second; 0 until the first window closed.
    pub fn rate_bps(&mut self, now: Instant) -> u64 {
        if let Some(started) = self.started {
            let elapsed = now.saturating_duration_since(started);
            if elapsed >= self.window {
                self.close_window(elapsed);
                self.started = Some(now);
            }
        }
        self.rate_bps.unwrap_or(0.0) as u64
    }
}

/// Everything the receiving side measures. Feed it every datagram and every
/// reassembled frame, then call [`ReceiverTelemetry::report`] periodically.
#[derive(Debug, Default)]
pub struct ReceiverTelemetry {
    sequences: [SequenceTracker; MediaKind::COUNT],
    jitter: JitterEstimator,
    bandwidth: BandwidthEstimator,
}

impl ReceiverTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a raw media datagram. Malformed datagrams count for bandwidth only.
    pub fn on_datagram(&mut self, datagram: &[u8], now: Instant) {
        self.bandwidth.record(datagram.len(), now);
        if let Ok((header, _)) = decode_datagram(datagram) {
            self.sequences[header.kind.index()].record(header.seq);
        }
    }

    /// Records a reassembled video frame for jitter.
    pub fn on_frame(&mut self, frame: &Frame, now: Instant) {
        if frame.kind == MediaKind::Video {
            self.jitter.record(frame.timestamp_us, now);
        }
    }

    pub fn sequence(&self, kind: MediaKind) -> &SequenceTracker {
        &self.sequences[kind.index()]
    }

    /// Builds the report for the sender and starts a new loss interval.
    pub fn report(&mut self, rtt: Duration, now: Instant) -> TelemetryReport {
        let mut loss = LossSample::default();
        for tracker in &mut self.sequences {
            let s = tracker.take_interval();
            loss.expected += s.expected;
            loss.received += s.received;
        }
        TelemetryReport {
            rtt_us: u32::try_from(rtt.as_micros()).unwrap_or(u32::MAX),
            jitter_us: u32::try_from(self.jitter.jitter().as_micros()).unwrap_or(u32::MAX),
            loss_ppm: loss.loss_ppm(),
            bandwidth_bps: self.bandwidth.rate_bps(now),
        }
    }
}

/// QUIC path statistics at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathSnapshot {
    pub rtt: Duration,
    pub min_rtt: Duration,
    pub cwnd: u64,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
}

impl PathSnapshot {
    pub fn of(connection: &Connection) -> Self {
        let path = connection.stats().path;
        Self {
            rtt: path.rtt,
            min_rtt: path.min_rtt,
            cwnd: path.cwnd,
            sent_packets: path.sent_packets,
            lost_packets: path.lost_packets,
            congestion_events: path.congestion_events,
        }
    }

    /// Bandwidth the congestion controller allows: one window per RTT.
    pub fn send_capacity_bps(&self) -> u64 {
        let rtt = self.rtt.as_secs_f64();
        if rtt <= 0.0 {
            return 0;
        }
        (self.cwnd as f64 * 8.0 / rtt) as u64
    }
}

/// Sender-side packet loss between two snapshots.
#[derive(Debug, Clone, Default)]
pub struct PathLossMeter {
    last: Option<PathSnapshot>,
}

impl PathLossMeter {
    /// Loss since the previous sample (all of the connection's life on the first call).
    pub fn sample(&mut self, now: PathSnapshot) -> LossSample {
        let (sent0, lost0) = self
            .last
            .map(|p| (p.sent_packets, p.lost_packets))
            .unwrap_or((0, 0));
        self.last = Some(now);
        let sent = now.sent_packets.saturating_sub(sent0);
        let lost = now.lost_packets.saturating_sub(lost0);
        LossSample {
            expected: sent,
            received: sent.saturating_sub(lost),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use v6cast_protocol::{
        ControlMessage, Fragmenter, MediaFlags, Push, Reassembler, IPV6_SAFE_DATAGRAM,
    };

    use super::*;

    #[test]
    fn in_order_stream_has_no_loss() {
        let mut t = SequenceTracker::new();
        for s in 0..5000 {
            t.record(s);
        }
        let i = t.take_interval();
        assert_eq!((i.expected, i.received, i.loss_ppm()), (5000, 5000, 0));
    }

    #[test]
    fn gaps_count_as_loss() {
        let mut t = SequenceTracker::new();
        for s in (0..1000).filter(|s| s % 10 != 9) {
            t.record(s);
        }
        let i = t.take_interval();
        // Highest seen is 998, so 0..=998 (999 numbers) are expected; 99 are missing.
        assert_eq!(i.expected, 999);
        assert_eq!(i.lost(), 99);
        assert_eq!(i.loss_ppm(), (99 * PPM / 999) as u32);
        assert_eq!(t.take_interval(), LossSample::default());
    }

    #[test]
    fn reordering_and_duplicates_are_not_loss() {
        let mut t = SequenceTracker::new();
        for s in [0, 2, 1, 4, 3, 3, 5, 5, 0] {
            t.record(s);
        }
        let i = t.take_interval();
        assert_eq!((i.expected, i.received), (6, 6));
        assert_eq!(t.duplicates(), 3);
    }

    #[test]
    fn late_arrival_in_the_next_interval_is_credited() {
        let mut t = SequenceTracker::new();
        t.record(0);
        t.record(2);
        assert_eq!(t.take_interval().lost(), 1);
        t.record(1);
        assert_eq!(t.total().lost(), 0);
    }

    #[test]
    fn wraps_and_resyncs() {
        let mut t = SequenceTracker::new();
        for s in [u32::MAX - 1, u32::MAX, 0, 1] {
            t.record(s);
        }
        assert_eq!(t.take_interval().lost(), 0);
        // Sender restarted far away: reset, not millions of lost packets.
        t.record(1_000_000);
        t.record(1_000_001);
        assert_eq!(t.take_interval().lost(), 0);
        // Behind the window: ignored.
        t.record(1_000_001 - SequenceTracker::WINDOW as u32 - 1);
        assert_eq!(t.too_old(), 1);
    }

    #[test]
    fn jitter_tracks_transit_variation() {
        let start = Instant::now();
        let mut steady = JitterEstimator::default();
        for i in 0..100u64 {
            steady.record(i * 16_667, start + Duration::from_micros(i * 16_667));
        }
        assert_eq!(steady.jitter(), Duration::ZERO);
        let mut noisy = JitterEstimator::default();
        for i in 0..100u64 {
            let wobble = if i % 2 == 0 { 0 } else { 4_000 };
            noisy.record(
                i * 16_667,
                start + Duration::from_micros(i * 16_667 + wobble),
            );
        }
        let j = noisy.jitter().as_micros();
        assert!((3_000..=4_000).contains(&j), "jitter {j} us");
    }

    #[test]
    fn bandwidth_estimate_converges() {
        let start = Instant::now();
        let mut bw = BandwidthEstimator::default();
        // 1250 bytes every millisecond = 10 Mbit/s.
        for ms in 0..2000u64 {
            bw.record(1250, start + Duration::from_millis(ms));
        }
        let rate = bw.rate_bps(start + Duration::from_millis(2000));
        assert!((9_500_000..=10_500_000).contains(&rate), "rate {rate}");
        assert_eq!(BandwidthEstimator::default().rate_bps(start), 0);
    }

    #[test]
    fn path_loss_meter_uses_deltas() {
        let snap = |sent, lost| PathSnapshot {
            rtt: Duration::from_millis(20),
            min_rtt: Duration::from_millis(18),
            cwnd: 50_000,
            sent_packets: sent,
            lost_packets: lost,
            congestion_events: 0,
        };
        let mut m = PathLossMeter::default();
        assert_eq!(m.sample(snap(100, 1)).lost(), 1);
        let s = m.sample(snap(300, 11));
        assert_eq!((s.expected, s.lost(), s.loss_ppm()), (200, 10, 50_000));
        assert_eq!(snap(0, 0).send_capacity_bps(), 20_000_000);
    }

    #[tokio::test]
    async fn loopback_report_carries_rtt_and_loss() {
        let (_server, _client, streamer, viewer) = crate::endpoint::tests::pair().await;

        // 120 single-fragment frames at a 60 Hz cadence; the sender drops every
        // tenth datagram on purpose, so the viewer must report 10 % loss.
        let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
        let mut sent = 0;
        for i in 0..120u64 {
            let flags = if i == 0 {
                MediaFlags::keyframe()
            } else {
                MediaFlags::default()
            };
            let datagram = f.fragment(&[i as u8; 600], flags, i * 16_667).unwrap();
            if i % 10 != 9 {
                streamer
                    .send_datagram(Bytes::from(datagram[0].clone()))
                    .unwrap();
                sent += 1;
            }
            if i % 20 == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        let mut telemetry = ReceiverTelemetry::new();
        let mut reassembler = Reassembler::default();
        for _ in 0..sent {
            let d = tokio::time::timeout(Duration::from_secs(5), viewer.recv_datagram())
                .await
                .expect("datagram timeout")
                .unwrap();
            let now = Instant::now();
            telemetry.on_datagram(&d, now);
            if let Push::Complete(frame) = reassembler.push_datagram(&d, now) {
                telemetry.on_frame(&frame, now);
            }
        }
        let report = telemetry.report(viewer.connection().rtt(), Instant::now());
        assert!(report.rtt_us > 0, "rtt {}", report.rtt_us);
        assert!(report.rtt_us < 200_000, "loopback rtt {} us", report.rtt_us);
        // 120 sent with seq 0..=119, 108 arrived; 119 never arrives, so 119 expected.
        let expected = (119 - 108) * PPM / 119;
        assert_eq!(u64::from(report.loss_ppm), expected);

        // The report travels back on the control stream.
        let msg = ControlMessage::TelemetryReport(report);
        viewer.send_control(&msg).await.unwrap();
        assert_eq!(streamer.recv_control().await.unwrap(), msg);

        let path = PathSnapshot::of(streamer.connection());
        assert!(path.rtt > Duration::ZERO);
        assert!(path.sent_packets > 0);
        assert!(path.send_capacity_bps() > 0);
    }
}
