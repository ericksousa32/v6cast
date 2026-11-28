//! One streamer, several viewers: every viewer gets the same frames, and a viewer
//! that cannot keep up never delays the others.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use common::*;
use v6cast_net::fanout::SinkError;
use v6cast_net::{DatagramSink, FanOut, Session, TransportSettings};
use v6cast_protocol::{
    Fragmenter, Frame, MediaFlags, MediaKind, Push, Reassembler, IPV6_SAFE_DATAGRAM,
};

/// A viewer whose outgoing buffer the test controls, standing in for a path that
/// cannot take more data (loopback never backs up on its own).
#[derive(Default)]
struct ScriptedSink {
    space: AtomicUsize,
    closed: AtomicBool,
    received: Mutex<Vec<Bytes>>,
}

impl ScriptedSink {
    fn new(space: usize) -> Arc<Self> {
        let sink = Self::default();
        sink.space.store(space, Ordering::SeqCst);
        Arc::new(sink)
    }
}

impl DatagramSink for ScriptedSink {
    fn send_space(&self) -> usize {
        self.space.load(Ordering::SeqCst)
    }

    fn send(&self, datagram: Bytes) -> Result<(), SinkError> {
        self.received.lock().unwrap().push(datagram);
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Lets one `FanOut` hold real QUIC sessions and scripted ones together.
enum Viewer {
    Quic(Arc<Session>),
    Scripted(Arc<ScriptedSink>),
}

impl DatagramSink for Viewer {
    fn send_space(&self) -> usize {
        match self {
            Viewer::Quic(s) => s.send_space(),
            Viewer::Scripted(s) => s.send_space(),
        }
    }

    fn send(&self, datagram: Bytes) -> Result<(), SinkError> {
        match self {
            Viewer::Quic(s) => DatagramSink::send(s.as_ref(), datagram),
            Viewer::Scripted(s) => s.send(datagram),
        }
    }

    fn is_closed(&self) -> bool {
        match self {
            Viewer::Quic(s) => s.is_closed(),
            Viewer::Scripted(s) => s.is_closed(),
        }
    }
}

struct Room {
    server: v6cast_net::Server,
    _clients: Vec<v6cast_net::Client>,
    /// What each viewer holds.
    viewers: Vec<Arc<Session>>,
    /// What the streamer holds, in the same order.
    streamer: Vec<Arc<Session>>,
}

async fn room(viewers: usize, transport: TransportSettings) -> Room {
    let mut server = server_with(transport);
    let addr = server.local_addr().unwrap();
    let mut clients = Vec::new();
    let (mut mine, mut theirs) = (Vec::new(), Vec::new());
    for n in 0..viewers {
        let c = client_named(TOKEN, &format!("viewer{n}"));
        mine.push(Arc::new(within("connect", c.connect(addr)).await.unwrap()));
        theirs.push(Arc::new(within("accept", server.accept()).await.unwrap()));
        clients.push(c);
    }
    Room {
        server,
        _clients: clients,
        viewers: mine,
        streamer: theirs,
    }
}

fn frame_data(i: u64, len: usize) -> Vec<u8> {
    (0..len).map(|k| (i as usize * 13 + k) as u8).collect()
}

fn fragment(f: &mut Fragmenter, data: &[u8], keyframe: bool, ts: u64) -> Vec<Bytes> {
    let flags = if keyframe {
        MediaFlags::keyframe()
    } else {
        MediaFlags::default()
    };
    f.fragment(data, flags, ts)
        .unwrap()
        .into_iter()
        .map(Bytes::from)
        .collect()
}

fn video() -> Fragmenter {
    Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap()
}

/// Reassembles `want` frames from a viewer session.
async fn collect(session: Arc<Session>, want: usize) -> Vec<Frame> {
    let mut reassembler = Reassembler::default();
    let mut frames = Vec::new();
    while frames.len() < want {
        let d = within("datagram", session.recv_datagram()).await.unwrap();
        if let Push::Complete(frame) = reassembler.push_datagram(&d, std::time::Instant::now()) {
            frames.push(frame);
        }
    }
    frames
}

#[tokio::test]
async fn three_viewers_receive_identical_streams() {
    if !ipv6_or_skip() {
        return;
    }
    let room = room(3, TransportSettings::default()).await;
    let fan: FanOut<Session> = FanOut::new();
    for s in &room.streamer {
        fan.add(s.clone());
    }
    let readers: Vec<_> = room
        .viewers
        .iter()
        .map(|v| tokio::spawn(collect(v.clone(), 40)))
        .collect();

    let mut f = video();
    for i in 0..40u64 {
        let len = if i % 20 == 0 { 15_000 } else { 2_500 };
        let report = fan
            .broadcast(&fragment(&mut f, &frame_data(i, len), i % 20 == 0, i))
            .unwrap();
        assert_eq!(
            (report.delivered, report.skipped, report.removed),
            (3, 0, 0),
            "frame {i}"
        );
        assert!(!report.keyframe_wanted);
        tokio::task::yield_now().await; // let the transport run, as a 60 Hz loop would
    }

    for (n, reader) in readers.into_iter().enumerate() {
        let frames = within("viewer", reader).await.unwrap();
        for (i, frame) in frames.iter().enumerate() {
            let len = if i % 20 == 0 { 15_000 } else { 2_500 };
            assert_eq!(
                frame.data,
                frame_data(i as u64, len),
                "viewer {n} frame {i}"
            );
            assert_eq!(frame.timestamp_us, i as u64);
        }
    }
    drop(room.server);
}

#[tokio::test]
async fn a_stalled_viewer_skips_frames_and_the_others_lose_nothing() {
    if !ipv6_or_skip() {
        return;
    }
    let room = room(2, TransportSettings::default()).await;
    let slow = ScriptedSink::new(1 << 20);
    let fan: FanOut<Viewer> = FanOut::new();
    for s in &room.streamer {
        fan.add(Arc::new(Viewer::Quic(s.clone())));
    }
    let slow_id = fan.add(Arc::new(Viewer::Scripted(slow.clone())));
    let readers: Vec<_> = room
        .viewers
        .iter()
        .map(|v| tokio::spawn(collect(v.clone(), 80)))
        .collect();

    // Keyframes at 0, 30 and 60. The slow viewer is healthy for frames 0..10,
    // stalled for 10..40 and healthy again from 40 on.
    let mut f = video();
    let mut skipped_for_slow = 0;
    for i in 0..80u64 {
        match i {
            10 => slow.space.store(0, Ordering::SeqCst),
            40 => slow.space.store(1 << 20, Ordering::SeqCst),
            _ => {}
        }
        let keyframe = i % 30 == 0;
        let report = fan
            .broadcast(&fragment(&mut f, &frame_data(i, 3_000), keyframe, i))
            .unwrap();
        // The two real viewers always get the frame, whatever the third one does.
        assert!(report.delivered >= 2, "frame {i}: {report:?}");
        assert_eq!(report.removed, 0);
        if report.skipped == 1 {
            skipped_for_slow += 1;
            assert!(
                report.keyframe_wanted,
                "frame {i}: the sender must be told to force an IDR"
            );
        } else {
            assert_eq!(report.delivered, 3, "frame {i}");
        }
        tokio::task::yield_now().await;
    }
    // Frames 10..60 are lost for the slow viewer: 10..40 for lack of space, then
    // 40..60 because delta frames are useless without the keyframe it missed.
    assert_eq!(skipped_for_slow, 50);
    let stats = fan.stats(slow_id).unwrap();
    assert_eq!((stats.frames_sent, stats.frames_skipped), (30, 50));

    for reader in readers {
        let frames = within("viewer", reader).await.unwrap();
        assert!(frames
            .iter()
            .enumerate()
            .all(|(i, f)| f.data == frame_data(i as u64, 3_000)));
    }

    // What the slow viewer did get is whole frames only: 0..10 and 60..80.
    let mut r = Reassembler::default();
    let mut ids = Vec::new();
    for d in slow.received.lock().unwrap().iter() {
        if let Push::Complete(frame) = r.push_datagram(d, std::time::Instant::now()) {
            ids.push(frame.frame_id);
        }
    }
    assert_eq!(ids, (0..10).chain(60..80).collect::<Vec<u32>>());
    drop(room.server);
}

#[tokio::test]
async fn audio_keeps_flowing_to_a_viewer_that_waits_for_a_keyframe() {
    if !ipv6_or_skip() {
        return;
    }
    let slow = ScriptedSink::new(0); // cannot take video right now
    let fan: FanOut<ScriptedSink> = FanOut::new();
    let id = fan.add(slow.clone());
    let mut v = video();
    let mut a = Fragmenter::new(MediaKind::Audio, 600).unwrap();

    let report = fan
        .broadcast(&fragment(&mut v, &[1; 2_000], true, 0))
        .unwrap();
    assert_eq!((report.delivered, report.skipped), (0, 1));
    slow.space.store(1 << 20, Ordering::SeqCst);

    // Video is held back until a keyframe, audio is not.
    let report = fan
        .broadcast(&fragment(&mut v, &[2; 2_000], false, 1))
        .unwrap();
    assert_eq!(
        (report.delivered, report.skipped, report.keyframe_wanted),
        (0, 1, true)
    );
    let report = fan
        .broadcast(&fragment(&mut a, &[3; 400], false, 1))
        .unwrap();
    assert_eq!((report.delivered, report.skipped), (1, 0));
    let report = fan
        .broadcast(&fragment(&mut v, &[4; 2_000], true, 2))
        .unwrap();
    assert_eq!((report.delivered, report.keyframe_wanted), (1, false));
    assert_eq!(fan.stats(id).unwrap().frames_sent, 2);
}

#[tokio::test]
async fn a_full_send_buffer_skips_whole_frames_never_fragments() {
    if !ipv6_or_skip() {
        return;
    }
    // A 32 KiB outgoing buffer and a sender that never yields, so the transport
    // cannot drain it: the buffer really fills up, on a real QUIC connection.
    let room = room(
        1,
        TransportSettings {
            datagram_buffer: 32 * 1024,
            ..TransportSettings::default()
        },
    )
    .await;
    let fan: FanOut<Session> = FanOut::new();
    fan.add(room.streamer[0].clone());

    let mut f = video();
    let (mut delivered, mut skipped) = (0usize, 0usize);
    let mut sent_ids = Vec::new();
    for i in 0..200u64 {
        let keyframe = i % 10 == 0;
        let report = fan
            .broadcast(&fragment(&mut f, &frame_data(i, 6_000), keyframe, i))
            .unwrap();
        assert_eq!(report.delivered + report.skipped, 1);
        delivered += report.delivered;
        skipped += report.skipped;
        if report.delivered == 1 {
            sent_ids.push(i as u32);
        }
    }
    assert!(delivered >= 1, "nothing fit in the buffer");
    assert!(
        skipped >= 100,
        "the buffer never filled: {delivered} delivered, {skipped} skipped"
    );

    // The viewer gets exactly the frames reported as delivered, each one whole.
    let frames = within("viewer", collect(room.viewers[0].clone(), delivered)).await;
    assert_eq!(
        frames.iter().map(|f| f.frame_id).collect::<Vec<_>>(),
        sent_ids
    );
    for frame in &frames {
        assert_eq!(frame.data, frame_data(u64::from(frame.frame_id), 6_000));
    }
}

#[tokio::test]
async fn a_viewer_that_leaves_is_removed_and_the_rest_carry_on() {
    if !ipv6_or_skip() {
        return;
    }
    let mut room = room(3, TransportSettings::default()).await;
    let fan: FanOut<Session> = FanOut::new();
    for s in &room.streamer {
        fan.add(s.clone());
    }
    let mut f = video();
    let report = fan
        .broadcast(&fragment(&mut f, &[1; 3_000], true, 0))
        .unwrap();
    assert_eq!(report.delivered, 3);

    // Viewer 1 goes away without a word: both of its handles are dropped.
    let leaver = room.viewers.remove(1);
    drop(leaver);
    room._clients.remove(1);
    let gone = room.streamer[1].clone();
    within("server notices the departure", gone.closed()).await;
    drop(gone);

    let report = fan
        .broadcast(&fragment(&mut f, &[2; 3_000], true, 1))
        .unwrap();
    assert_eq!((report.delivered, report.removed), (2, 1), "{report:?}");
    assert_eq!(fan.len(), 2);
    // The two that stayed received both frames.
    for viewer in &room.viewers {
        let frames = within("viewer", collect(viewer.clone(), 2)).await;
        assert_eq!(frames[1].data, vec![2u8; 3_000]);
    }
}

#[tokio::test]
async fn a_late_joiner_starts_at_the_next_keyframe() {
    if !ipv6_or_skip() {
        return;
    }
    let room = room(2, TransportSettings::default()).await;
    let fan: FanOut<Session> = FanOut::new();
    fan.add(room.streamer[0].clone());
    let mut f = video();

    for i in 0..10u64 {
        fan.broadcast(&fragment(&mut f, &frame_data(i, 3_000), i == 0, i))
            .unwrap();
    }
    fan.add(room.streamer[1].clone()); // joins in the middle of a group of pictures
    for i in 10..20u64 {
        let report = fan
            .broadcast(&fragment(&mut f, &frame_data(i, 3_000), false, i))
            .unwrap();
        assert_eq!(
            (report.delivered, report.skipped, report.keyframe_wanted),
            (1, 1, true),
            "frame {i}"
        );
    }
    let report = fan
        .broadcast(&fragment(&mut f, &frame_data(20, 3_000), true, 20))
        .unwrap();
    assert_eq!(
        (report.delivered, report.skipped, report.keyframe_wanted),
        (2, 0, false)
    );
    fan.broadcast(&fragment(&mut f, &frame_data(21, 3_000), false, 21))
        .unwrap();

    let first = within("early viewer", collect(room.viewers[0].clone(), 22)).await;
    assert_eq!(first.len(), 22);
    let joiner = within("late viewer", collect(room.viewers[1].clone(), 2)).await;
    assert_eq!(
        joiner.iter().map(|f| f.frame_id).collect::<Vec<_>>(),
        [20, 21]
    );
    assert!(joiner[0].flags.is_keyframe());
}

#[tokio::test]
async fn a_closed_viewer_is_removed_even_when_it_has_no_buffer_space() {
    // A dead connection must not linger as a "slow viewer" that is skipped forever.
    let dead = ScriptedSink::new(0);
    dead.closed.store(true, Ordering::SeqCst);
    let fan: FanOut<ScriptedSink> = FanOut::new();
    fan.add(dead);
    fan.add(ScriptedSink::new(1 << 20));

    let mut f = video();
    let report = fan
        .broadcast(&fragment(&mut f, &[1; 2_000], true, 0))
        .unwrap();
    assert_eq!(
        (report.delivered, report.skipped, report.removed),
        (1, 0, 1),
        "{report:?}"
    );
    assert_eq!(fan.len(), 1);
}
