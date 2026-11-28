//! Unicast fan-out: one encoded stream copied to every connected viewer.
//!
//! A frame is fragmented once; every viewer gets the same [`Bytes`] buffers
//! (reference counted, no copy, no re-encode). Sending never waits:
//!
//! * a viewer whose outgoing datagram buffer cannot hold the whole frame skips
//!   that frame, so a slow viewer never delays the others and never receives a
//!   frame it could not complete;
//! * after a skipped video frame, and when a viewer joins, the viewer receives no
//!   video until the next keyframe, because delta frames would not decode. The
//!   [`BroadcastReport`] says when a keyframe is wanted, so the sender can force
//!   an IDR;
//! * audio frames are independent and are never gated;
//! * viewers whose connection closed are removed.
//!
//! Fragment with [`v6cast_protocol::IPV6_SAFE_DATAGRAM`] so the datagrams fit
//! every viewer's path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use v6cast_protocol::{decode_datagram, MediaKind, ProtocolError};

use crate::session::Session;

/// Why a datagram could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// The connection is gone.
    Closed,
    /// The datagram exceeds what the path accepts.
    TooLarge,
    /// The peer does not accept datagrams.
    Unsupported,
}

/// Something that queues datagrams without blocking.
pub trait DatagramSink: Send + Sync {
    /// Free bytes in the outgoing datagram buffer.
    fn send_space(&self) -> usize;
    /// Queues one datagram. Must not block.
    fn send(&self, datagram: Bytes) -> Result<(), SinkError>;
    fn is_closed(&self) -> bool;
}

impl DatagramSink for quinn::Connection {
    fn send_space(&self) -> usize {
        self.datagram_send_buffer_space()
    }

    fn send(&self, datagram: Bytes) -> Result<(), SinkError> {
        self.send_datagram(datagram).map_err(|e| match e {
            quinn::SendDatagramError::TooLarge => SinkError::TooLarge,
            quinn::SendDatagramError::ConnectionLost(_) => SinkError::Closed,
            _ => SinkError::Unsupported,
        })
    }

    fn is_closed(&self) -> bool {
        self.close_reason().is_some()
    }
}

impl DatagramSink for Session {
    fn send_space(&self) -> usize {
        self.connection().send_space()
    }

    fn send(&self, datagram: Bytes) -> Result<(), SinkError> {
        DatagramSink::send(self.connection(), datagram)
    }

    fn is_closed(&self) -> bool {
        self.connection().is_closed()
    }
}

/// Handle of a viewer in a [`FanOut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberId(u64);

/// Per-viewer counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriberStats {
    pub frames_sent: u64,
    /// Frames not sent: buffer full, or waiting for a keyframe.
    pub frames_skipped: u64,
    pub datagrams_sent: u64,
    pub bytes_sent: u64,
}

/// What happened to one broadcast frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BroadcastReport {
    pub delivered: usize,
    pub skipped: usize,
    /// Viewers removed because their connection closed.
    pub removed: usize,
    /// At least one viewer waits for a video keyframe.
    pub keyframe_wanted: bool,
}

struct Subscriber<S> {
    id: SubscriberId,
    sink: Arc<S>,
    stats: SubscriberStats,
    waiting_for_keyframe: bool,
}

/// Copies frames to every subscribed viewer. See the module docs.
pub struct FanOut<S: DatagramSink = Session> {
    subscribers: Mutex<Vec<Subscriber<S>>>,
    next_id: AtomicU64,
}

impl<S: DatagramSink> Default for FanOut<S> {
    fn default() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }
}

impl<S: DatagramSink> std::fmt::Debug for FanOut<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FanOut")
            .field("subscribers", &self.len())
            .finish()
    }
}

impl<S: DatagramSink> FanOut<S> {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Subscriber<S>>> {
        // A panic while holding the lock leaves the list consistent; keep going.
        self.subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Adds a viewer. It receives video from the next keyframe on.
    pub fn add(&self, sink: Arc<S>) -> SubscriberId {
        let id = SubscriberId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.lock().push(Subscriber {
            id,
            sink,
            stats: SubscriberStats::default(),
            waiting_for_keyframe: true,
        });
        id
    }

    pub fn remove(&self, id: SubscriberId) -> bool {
        let mut subs = self.lock();
        let before = subs.len();
        subs.retain(|s| s.id != id);
        subs.len() != before
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stats(&self, id: SubscriberId) -> Option<SubscriberStats> {
        self.lock().iter().find(|s| s.id == id).map(|s| s.stats)
    }

    /// Sends every datagram of one frame to each viewer, without blocking.
    ///
    /// `datagrams` are the fragments of a single frame, as produced by
    /// [`v6cast_protocol::Fragmenter`]. The first header decides the media kind
    /// and whether the frame is a keyframe.
    pub fn broadcast(&self, datagrams: &[Bytes]) -> Result<BroadcastReport, ProtocolError> {
        let first = datagrams.first().ok_or(ProtocolError::InvalidField {
            field: "datagrams",
            reason: "empty frame",
        })?;
        let (header, _) = decode_datagram(first)?;
        let independent = header.kind == MediaKind::Audio || header.flags.is_keyframe();
        let is_video = header.kind == MediaKind::Video;
        let total: usize = datagrams.iter().map(Bytes::len).sum();

        let mut report = BroadcastReport::default();
        let mut subs = self.lock();
        subs.retain_mut(|sub| {
            if sub.sink.is_closed() {
                report.removed += 1;
                return false;
            }
            let gated = is_video && sub.waiting_for_keyframe && !independent;
            if gated || sub.sink.send_space() < total {
                sub.stats.frames_skipped += 1;
                if is_video {
                    sub.waiting_for_keyframe = true;
                }
                report.skipped += 1;
                return true;
            }
            for datagram in datagrams {
                match sub.sink.send(datagram.clone()) {
                    Ok(()) => {
                        sub.stats.datagrams_sent += 1;
                        sub.stats.bytes_sent += datagram.len() as u64;
                    }
                    Err(SinkError::Closed) => {
                        report.removed += 1;
                        return false;
                    }
                    Err(SinkError::TooLarge | SinkError::Unsupported) => {
                        // The rest of the frame is useless to this viewer.
                        sub.stats.frames_skipped += 1;
                        if is_video {
                            sub.waiting_for_keyframe = true;
                        }
                        report.skipped += 1;
                        return true;
                    }
                }
            }
            sub.stats.frames_sent += 1;
            if is_video && independent {
                sub.waiting_for_keyframe = false;
            }
            report.delivered += 1;
            true
        });
        report.keyframe_wanted = subs.iter().any(|s| s.waiting_for_keyframe);
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Instant;

    use v6cast_protocol::{Fragmenter, MediaFlags, Push, Reassembler, IPV6_SAFE_DATAGRAM};

    use super::*;

    #[derive(Default)]
    struct FakeSink {
        space: AtomicUsize,
        closed: AtomicBool,
        received: Mutex<Vec<Bytes>>,
    }

    impl FakeSink {
        fn with_space(space: usize) -> Arc<Self> {
            let s = Self::default();
            s.space.store(space, Ordering::Relaxed);
            Arc::new(s)
        }

        fn count(&self) -> usize {
            self.received.lock().unwrap().len()
        }
    }

    impl DatagramSink for FakeSink {
        fn send_space(&self) -> usize {
            self.space.load(Ordering::Relaxed)
        }

        fn send(&self, datagram: Bytes) -> Result<(), SinkError> {
            if self.closed.load(Ordering::Relaxed) {
                return Err(SinkError::Closed);
            }
            self.received.lock().unwrap().push(datagram);
            Ok(())
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::Relaxed)
        }
    }

    fn frame(f: &mut Fragmenter, len: usize, keyframe: bool) -> Vec<Bytes> {
        let flags = if keyframe {
            MediaFlags::keyframe()
        } else {
            MediaFlags::default()
        };
        f.fragment(&vec![7u8; len], flags, 0)
            .unwrap()
            .into_iter()
            .map(Bytes::from)
            .collect()
    }

    fn video() -> Fragmenter {
        Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap()
    }

    #[test]
    fn every_viewer_shares_the_same_buffers() {
        let fan = FanOut::new();
        let a = FakeSink::with_space(usize::MAX);
        let b = FakeSink::with_space(usize::MAX);
        fan.add(a.clone());
        fan.add(b.clone());
        let d = frame(&mut video(), 5000, true);
        let report = fan.broadcast(&d).unwrap();
        assert_eq!(report.delivered, 2);
        let ra = a.received.lock().unwrap();
        let rb = b.received.lock().unwrap();
        assert_eq!(ra.len(), d.len());
        for ((x, y), orig) in ra.iter().zip(rb.iter()).zip(&d) {
            // Same allocation: copied by reference, not re-encoded.
            assert_eq!(x.as_ptr(), orig.as_ptr());
            assert_eq!(y.as_ptr(), orig.as_ptr());
        }
    }

    #[test]
    fn slow_viewer_skips_frames_without_affecting_others() {
        let fan = FanOut::new();
        let fast = FakeSink::with_space(usize::MAX);
        let slow = FakeSink::with_space(usize::MAX);
        let fast_id = fan.add(fast.clone());
        let slow_id = fan.add(slow.clone());
        let mut f = video();
        let key = frame(&mut f, 3000, true);
        fan.broadcast(&key).unwrap();
        assert_eq!(slow.count(), key.len());

        // The slow viewer's buffer fills up.
        slow.space.store(1000, Ordering::Relaxed);
        let delta = frame(&mut f, 3000, false);
        let report = fan.broadcast(&delta).unwrap();
        assert_eq!((report.delivered, report.skipped), (1, 1));
        assert!(report.keyframe_wanted);
        assert_eq!(fast.count(), key.len() + delta.len());
        assert_eq!(slow.count(), key.len(), "no partial frame was sent");

        // Room again, but the next delta would not decode: still skipped.
        slow.space.store(usize::MAX, Ordering::Relaxed);
        let report = fan.broadcast(&frame(&mut f, 3000, false)).unwrap();
        assert_eq!(report.skipped, 1);
        // The next keyframe resynchronises it.
        let report = fan.broadcast(&frame(&mut f, 3000, true)).unwrap();
        assert_eq!((report.delivered, report.keyframe_wanted), (2, false));

        assert_eq!(fan.stats(fast_id).unwrap().frames_sent, 4);
        let slow_stats = fan.stats(slow_id).unwrap();
        assert_eq!((slow_stats.frames_sent, slow_stats.frames_skipped), (2, 2));
    }

    #[test]
    fn new_viewer_starts_at_a_keyframe() {
        let fan = FanOut::new();
        let sink = FakeSink::with_space(usize::MAX);
        fan.add(sink.clone());
        let mut f = video();
        let report = fan.broadcast(&frame(&mut f, 100, false)).unwrap();
        assert_eq!(report.skipped, 1);
        assert!(report.keyframe_wanted);
        assert_eq!(sink.count(), 0);
        fan.broadcast(&frame(&mut f, 100, true)).unwrap();
        fan.broadcast(&frame(&mut f, 100, false)).unwrap();
        assert_eq!(sink.count(), 2);
    }

    #[test]
    fn audio_is_never_gated() {
        let fan = FanOut::new();
        let sink = FakeSink::with_space(usize::MAX);
        fan.add(sink.clone());
        let mut audio = Fragmenter::new(MediaKind::Audio, IPV6_SAFE_DATAGRAM).unwrap();
        let report = fan.broadcast(&frame(&mut audio, 200, false)).unwrap();
        assert_eq!(report.delivered, 1);
        // Audio does not clear the video keyframe wait.
        assert!(report.keyframe_wanted);
    }

    #[test]
    fn closed_viewers_are_removed() {
        let fan = FanOut::new();
        let open = FakeSink::with_space(usize::MAX);
        let gone = FakeSink::with_space(usize::MAX);
        fan.add(open.clone());
        let gone_id = fan.add(gone.clone());
        gone.closed.store(true, Ordering::Relaxed);
        let report = fan.broadcast(&frame(&mut video(), 100, true)).unwrap();
        assert_eq!((report.delivered, report.removed), (1, 1));
        assert_eq!(fan.len(), 1);
        assert!(fan.stats(gone_id).is_none());
        assert!(!fan.remove(gone_id));
    }

    #[test]
    fn malformed_frames_are_refused() {
        let fan: FanOut<FakeSink> = FanOut::new();
        assert!(fan.broadcast(&[]).is_err());
        assert!(fan.broadcast(&[Bytes::from_static(b"junk")]).is_err());
    }

    #[tokio::test]
    async fn two_quic_viewers_receive_the_same_frame() {
        use crate::endpoint::tests::{client, server_with};
        use crate::TransportSettings;

        let mut server = server_with(TransportSettings::default());
        let addr = server.local_addr().unwrap();
        let c1 = client("correct horse");
        let c2 = client("correct horse");
        let v1 = c1.connect(addr).await.unwrap();
        let v2 = c2.connect(addr).await.unwrap();
        let fan = FanOut::new();
        fan.add(Arc::new(server.accept().await.unwrap()));
        fan.add(Arc::new(server.accept().await.unwrap()));

        let data: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        let mut f = video();
        let d: Vec<Bytes> = f
            .fragment(&data, MediaFlags::keyframe(), 99)
            .unwrap()
            .into_iter()
            .map(Bytes::from)
            .collect();
        assert_eq!(fan.broadcast(&d).unwrap().delivered, 2);

        for viewer in [&v1, &v2] {
            let mut r = Reassembler::default();
            let frame = loop {
                let datagram =
                    tokio::time::timeout(std::time::Duration::from_secs(5), viewer.recv_datagram())
                        .await
                        .expect("datagram timeout")
                        .unwrap();
                if let Push::Complete(frame) = r.push_datagram(&datagram, Instant::now()) {
                    break frame;
                }
            };
            assert_eq!(frame.data, data);
            assert_eq!(frame.timestamp_us, 99);
        }
    }
}
