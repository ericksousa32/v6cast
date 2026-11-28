//! Fragmentation and reassembly under loss, reordering and duplication.
//!
//! The main test replays randomised arrival orders against a small independent
//! model of the delivery rules, so the reassembler is checked against the
//! specification and not against itself.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use v6cast_protocol::fragment::MIN_DATAGRAM;
use v6cast_protocol::media::MAX_FRAME_LEN;
use v6cast_protocol::{
    decode_datagram, DropReason, Fragmenter, MediaFlags, MediaHeader, MediaKind, Push, Reassembler,
    ReassemblyConfig, IPV6_SAFE_DATAGRAM, MEDIA_HEADER_LEN,
};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

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

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }

    fn payload(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn is_newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}

/// A reassembler that never evicts or expires, so only the delivery rules apply.
fn unlimited() -> Reassembler {
    Reassembler::new(ReassemblyConfig {
        max_partial_frames: 100_000,
        max_buffered_bytes: usize::MAX / 4,
        max_frame_age: Duration::from_secs(3600),
    })
}

fn fragments(f: &mut Fragmenter, data: &[u8], flags: MediaFlags, ts: u64) -> Vec<Vec<u8>> {
    f.fragment(data, flags, ts).expect("fragment")
}

// ── the delivery model ───────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Expect {
    Late,
    Duplicate,
    Pending,
    Complete,
}

#[derive(Default)]
struct Model {
    last: [Option<u32>; 2],
    seen: HashMap<(usize, u32), HashSet<u16>>,
}

impl Model {
    fn expect(&mut self, datagram: &[u8]) -> Expect {
        let (h, _) = decode_datagram(datagram).expect("test datagrams are valid");
        let k = h.kind.index();
        if let Some(last) = self.last[k] {
            if !is_newer(h.frame_id, last) {
                return Expect::Late;
            }
        }
        let set = self.seen.entry((k, h.frame_id)).or_default();
        if !set.insert(h.fragment_index) {
            return Expect::Duplicate;
        }
        if set.len() < usize::from(h.fragment_count) {
            return Expect::Pending;
        }
        self.last[k] = Some(h.frame_id);
        self.seen
            .retain(|&(kind, id), _| kind != k || is_newer(id, h.frame_id));
        Expect::Complete
    }
}

struct Stream {
    originals: HashMap<(usize, u32), (Vec<u8>, MediaFlags, u64)>,
    datagrams: Vec<Vec<u8>>,
}

/// Interleaved video and audio frames of random sizes, already fragmented.
fn build_stream(rng: &mut Rng, frames: usize) -> Stream {
    let mut video = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let mut audio = Fragmenter::new(MediaKind::Audio, 600).unwrap();
    let mut originals = HashMap::new();
    let mut datagrams = Vec::new();
    let (mut video_id, mut audio_id) = (0u32, 0u32);
    for n in 0..frames {
        let ts = n as u64 * 16_667;
        if rng.chance(25) {
            let len = 1 + rng.below(1500) as usize;
            let data = rng.payload(len);
            datagrams.extend(fragments(&mut audio, &data, MediaFlags::default(), ts));
            originals.insert((1, audio_id), (data, MediaFlags::default(), ts));
            audio_id += 1;
        } else {
            let len = match rng.below(10) {
                0 => 1,
                1 => IPV6_SAFE_DATAGRAM - MEDIA_HEADER_LEN, // exactly one datagram
                2 => 20_000 + rng.below(30_000) as usize,   // keyframe-sized
                _ => 1 + rng.below(6_000) as usize,
            };
            let flags = if n % 30 == 0 {
                MediaFlags::keyframe()
            } else {
                MediaFlags::default()
            };
            let data = rng.payload(len);
            datagrams.extend(fragments(&mut video, &data, flags, ts));
            originals.insert((0, video_id), (data, flags, ts));
            video_id += 1;
        }
    }
    Stream {
        originals,
        datagrams,
    }
}

/// Drops, duplicates and reorders datagrams. `window` bounds how far a datagram
/// can move; 1 keeps the order.
fn disturb(
    rng: &mut Rng,
    datagrams: &[Vec<u8>],
    loss: u64,
    dup: u64,
    window: usize,
) -> Vec<Vec<u8>> {
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for d in datagrams {
        if rng.chance(loss) {
            continue;
        }
        kept.push(d.clone());
        if rng.chance(dup) {
            kept.push(d.clone());
        }
    }
    for chunk in kept.chunks_mut(window.max(1)) {
        rng.shuffle(chunk);
    }
    kept
}

#[test]
fn reassembler_matches_the_delivery_model_under_loss_reordering_and_duplicates() {
    let now = Instant::now();
    let mut total_complete = 0usize;
    let mut total_dropped_late = 0usize;
    for seed in 1..=60u64 {
        let mut rng = Rng::new(seed);
        let stream = build_stream(&mut rng, 80);
        let loss = [0, 0, 1, 5, 20, 45][seed as usize % 6];
        let dup = [0, 3, 0, 10][seed as usize % 4];
        let window = [1, 2, 5, 12, 40, 200][seed as usize % 6];
        let arrival = disturb(&mut rng, &stream.datagrams, loss, dup, window);

        let mut reassembler = unlimited();
        let mut model = Model::default();
        let mut last_complete: [Option<u32>; 2] = [None; 2];
        for (i, d) in arrival.iter().enumerate() {
            let expected = model.expect(d);
            let ctx = format!("seed {seed} loss {loss}% dup {dup}% window {window} datagram {i}");
            match (reassembler.push_datagram(d, now), expected) {
                (Push::Dropped(DropReason::Late), Expect::Late) => total_dropped_late += 1,
                (Push::Dropped(DropReason::Duplicate), Expect::Duplicate) => {}
                (Push::Pending, Expect::Pending) => {}
                (Push::Complete(frame), Expect::Complete) => {
                    total_complete += 1;
                    let k = frame.kind.index();
                    let (data, flags, ts) = &stream.originals[&(k, frame.frame_id)];
                    assert_eq!(&frame.data, data, "{ctx}: payload differs");
                    assert_eq!(frame.flags, *flags, "{ctx}");
                    assert_eq!(frame.timestamp_us, *ts, "{ctx}");
                    if let Some(prev) = last_complete[k] {
                        assert!(
                            is_newer(frame.frame_id, prev),
                            "{ctx}: delivered out of order"
                        );
                    }
                    last_complete[k] = Some(frame.frame_id);
                }
                (got, want) => panic!("{ctx}: reassembler said {got:?}, model said {want:?}"),
            }
        }
        // Nothing is left over once the stream ends and the stale frames are gone.
        reassembler.expire(now + Duration::from_secs(7200));
        assert_eq!(reassembler.partial_frames(), 0, "seed {seed}");
        assert_eq!(reassembler.buffered_bytes(), 0, "seed {seed}");
    }
    // Guard against a vacuous run: the scenarios did deliver and did reorder.
    assert!(total_complete > 1000, "{total_complete}");
    assert!(
        total_dropped_late > 0,
        "reordering never produced a late fragment"
    );
}

#[test]
fn clean_in_order_stream_delivers_every_frame_exactly_once() {
    let now = Instant::now();
    let mut rng = Rng::new(77);
    let stream = build_stream(&mut rng, 300);
    let mut r = Reassembler::default();
    let mut completed = 0;
    for d in &stream.datagrams {
        match r.push_datagram(d, now) {
            Push::Complete(_) => completed += 1,
            Push::Pending => {}
            Push::Dropped(why) => panic!("clean stream dropped a datagram: {why:?}"),
        }
    }
    assert_eq!(completed, stream.originals.len());
    assert_eq!(r.stats().frames_completed as usize, completed);
    assert_eq!(r.stats().frames_dropped, 0);
    assert_eq!(r.buffered_bytes(), 0);
}

#[test]
fn every_arrival_order_of_a_frame_completes_it_exactly_once() {
    fn permutations(items: &mut Vec<usize>, k: usize, out: &mut Vec<Vec<usize>>) {
        if k == items.len() {
            out.push(items.clone());
            return;
        }
        for i in k..items.len() {
            items.swap(k, i);
            permutations(items, k + 1, out);
            items.swap(k, i);
        }
    }

    let mut rng = Rng::new(5);
    let data = rng.payload(4 * (IPV6_SAFE_DATAGRAM - MEDIA_HEADER_LEN) + 17);
    let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let dgs = fragments(&mut f, &data, MediaFlags::keyframe(), 9);
    assert_eq!(dgs.len(), 5);

    let mut orders = Vec::new();
    permutations(&mut (0..dgs.len()).collect(), 0, &mut orders);
    assert_eq!(orders.len(), 120);
    for order in orders {
        let mut r = Reassembler::default();
        let now = Instant::now();
        let mut done = None;
        for (n, &i) in order.iter().enumerate() {
            match r.push_datagram(&dgs[i], now) {
                Push::Complete(frame) => {
                    assert_eq!(
                        n,
                        order.len() - 1,
                        "{order:?}: completed before the last fragment"
                    );
                    done = Some(frame);
                }
                Push::Pending => assert!(n < order.len() - 1, "{order:?}"),
                Push::Dropped(why) => panic!("{order:?}: {why:?}"),
            }
        }
        assert_eq!(done.expect("frame").data, data, "{order:?}");
        assert_eq!(r.buffered_bytes(), 0);
    }
}

#[test]
fn one_lost_fragment_loses_only_its_frame() {
    let now = Instant::now();
    let mut rng = Rng::new(11);
    let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let frames: Vec<Vec<u8>> = (0..10).map(|_| rng.payload(5000)).collect();
    let mut r = Reassembler::default();
    let mut delivered = Vec::new();
    for (n, data) in frames.iter().enumerate() {
        let mut dgs = fragments(&mut f, data, MediaFlags::default(), n as u64);
        if n == 4 {
            dgs.remove(2); // lose one fragment of frame 4
        }
        for d in &dgs {
            if let Push::Complete(frame) = r.push_datagram(d, now) {
                delivered.push(frame.frame_id);
            }
        }
    }
    assert_eq!(delivered, [0, 1, 2, 3, 5, 6, 7, 8, 9]);
    // Frame 4 was overtaken by frame 5 and its leftovers released.
    assert_eq!(r.partial_frames(), 0);
    assert_eq!(r.buffered_bytes(), 0);
    assert_eq!(r.stats().frames_dropped, 1);
}

#[test]
fn newer_frame_overtakes_an_older_incomplete_one() {
    let now = Instant::now();
    let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let old = fragments(&mut f, &vec![1u8; 4000], MediaFlags::default(), 0);
    let new = fragments(&mut f, &vec![2u8; 4000], MediaFlags::default(), 1);
    let mut r = Reassembler::default();
    assert_eq!(r.push_datagram(&old[0], now), Push::Pending);
    let mut last = Push::Pending;
    for d in &new {
        last = r.push_datagram(d, now);
    }
    assert!(matches!(last, Push::Complete(ref fr) if fr.data == vec![2u8; 4000]));
    // The straggling rest of the old frame is late, not buffered.
    for d in &old[1..] {
        assert_eq!(r.push_datagram(d, now), Push::Dropped(DropReason::Late));
    }
    assert_eq!(r.buffered_bytes(), 0);
}

#[test]
fn stale_partial_frames_expire_without_sleeping() {
    let t0 = Instant::now();
    let config = ReassemblyConfig {
        max_frame_age: Duration::from_millis(500),
        ..ReassemblyConfig::default()
    };
    let mut r = Reassembler::new(config);
    let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let stale = fragments(&mut f, &vec![7u8; 5000], MediaFlags::default(), 0);
    let fresh = fragments(&mut f, &vec![8u8; 5000], MediaFlags::default(), 1);

    assert_eq!(r.push_datagram(&stale[0], t0), Push::Pending);
    assert_eq!(r.partial_frames(), 1);
    // Exactly at the age limit it is still kept; one millisecond later it is gone.
    r.expire(t0 + Duration::from_millis(500));
    assert_eq!(r.partial_frames(), 1);
    r.expire(t0 + Duration::from_millis(501));
    assert_eq!(r.partial_frames(), 0);
    assert_eq!(r.buffered_bytes(), 0);
    assert_eq!(r.stats().frames_dropped, 1);

    // A frame that arrives after the clock moved on is unaffected.
    let late = t0 + Duration::from_secs(60);
    let mut got = None;
    for d in &fresh {
        if let Push::Complete(frame) = r.push_datagram(d, late) {
            got = Some(frame);
        }
    }
    assert_eq!(got.expect("complete").data, vec![8u8; 5000]);
}

#[test]
fn memory_stays_bounded_while_half_of_every_large_frame_is_lost() {
    let config = ReassemblyConfig::default();
    let mut r = Reassembler::new(config);
    let mut rng = Rng::new(21);
    let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let start = Instant::now();
    let mut peak = 0;
    for n in 0..1500u64 {
        let data = vec![n as u8; 60_000 + rng.below(100_000) as usize];
        for d in fragments(&mut f, &data, MediaFlags::default(), n) {
            if rng.chance(50) {
                continue;
            }
            // 60 frames per second of simulated time.
            r.push_datagram(&d, start + Duration::from_micros(n * 16_667));
            assert!(r.buffered_bytes() <= config.max_buffered_bytes);
            assert!(r.partial_frames() <= config.max_partial_frames);
            peak = peak.max(r.buffered_bytes());
        }
    }
    // Incomplete frames piled up and were discarded; otherwise this proves nothing.
    assert!(peak > 0);
    assert!(r.stats().frames_dropped > 100, "{:?}", r.stats());
}

#[test]
fn eviction_prefers_the_oldest_partial_frame() {
    let t0 = Instant::now();
    let config = ReassemblyConfig {
        max_partial_frames: 2,
        ..ReassemblyConfig::default()
    };
    let mut r = Reassembler::new(config);
    let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let frames: Vec<Vec<Vec<u8>>> = (0..3)
        .map(|n| fragments(&mut f, &vec![n as u8; 3000], MediaFlags::default(), n))
        .collect();
    for (n, dgs) in frames.iter().enumerate() {
        // First fragment of each frame only: three partial frames, room for two.
        r.push_datagram(&dgs[0], t0 + Duration::from_millis(n as u64));
    }
    assert_eq!(r.partial_frames(), 2);
    assert_eq!(r.stats().frames_dropped, 1);
    // The newest two survive and can still be completed.
    let mut completed = Vec::new();
    for dgs in &frames[1..] {
        for d in &dgs[1..] {
            if let Push::Complete(fr) = r.push_datagram(d, t0 + Duration::from_millis(10)) {
                completed.push(fr.frame_id);
            }
        }
    }
    assert_eq!(completed, [1, 2]);
}

#[test]
fn frame_ids_wrap_around_u32_in_order() {
    let now = Instant::now();
    let mut r = Reassembler::default();
    let one_fragment = |id: u32| {
        MediaHeader {
            kind: MediaKind::Video,
            flags: MediaFlags::default(),
            seq: id,
            frame_id: id,
            fragment_index: 0,
            fragment_count: 1,
            frame_len: 4,
            timestamp_us: u64::from(id),
        }
        .encode_datagram(&[id as u8; 4])
    };
    let ids = [u32::MAX - 1, u32::MAX, 0, 1, 2];
    for id in ids {
        match r.push_datagram(&one_fragment(id), now) {
            Push::Complete(frame) => assert_eq!(frame.frame_id, id),
            other => panic!("{id}: {other:?}"),
        }
    }
    // Anything at or before the last delivered id is late, on both sides of the wrap.
    for id in [u32::MAX, 0, 2] {
        assert_eq!(
            r.push_datagram(&one_fragment(id), now),
            Push::Dropped(DropReason::Late),
            "{id}"
        );
    }
    assert!(matches!(
        r.push_datagram(&one_fragment(3), now),
        Push::Complete(_)
    ));
}

#[test]
fn frames_of_every_boundary_size_round_trip_for_every_datagram_size() {
    let mut rng = Rng::new(31);
    for max_datagram in [MIN_DATAGRAM, 600, IPV6_SAFE_DATAGRAM, 1400] {
        let cap = max_datagram - MEDIA_HEADER_LEN;
        let mut sizes = vec![
            1,
            2,
            cap - 1,
            cap,
            cap + 1,
            2 * cap,
            2 * cap + 1,
            10 * cap + 7,
        ];
        sizes.push(MAX_FRAME_LEN as usize);
        let mut f = Fragmenter::new(MediaKind::Video, max_datagram).unwrap();
        for len in sizes {
            let data = rng.payload(len);
            let dgs = fragments(&mut f, &data, MediaFlags::keyframe(), len as u64);
            assert_eq!(
                dgs.len(),
                len.div_ceil(cap),
                "size {len} datagram {max_datagram}"
            );
            assert!(dgs.iter().all(|d| d.len() <= max_datagram));
            let mut r = Reassembler::default();
            let mut out = None;
            for d in &dgs {
                if let Push::Complete(frame) = r.push_datagram(d, Instant::now()) {
                    out = Some(frame);
                }
            }
            assert_eq!(
                out.expect("complete").data,
                data,
                "size {len} datagram {max_datagram}"
            );
        }
    }
}

#[test]
fn audio_loss_never_blocks_video() {
    let now = Instant::now();
    let mut video = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let mut audio = Fragmenter::new(MediaKind::Audio, 600).unwrap();
    let mut r = Reassembler::default();
    let mut video_done = 0;
    for n in 0..50u64 {
        // Audio frame 'n' loses its first fragment every time: it never completes.
        let a = fragments(&mut audio, &vec![3u8; 1200], MediaFlags::default(), n);
        for d in &a[1..] {
            r.push_datagram(d, now);
        }
        for d in fragments(&mut video, &vec![4u8; 3000], MediaFlags::default(), n) {
            if matches!(r.push_datagram(&d, now), Push::Complete(_)) {
                video_done += 1;
            }
        }
    }
    assert_eq!(video_done, 50);
    assert_eq!(r.last_delivered(MediaKind::Audio), None);
}

// ── known problems, kept as executable reports ───────────────────────────────

/// After the sender's frame counter restarts at 0 (new `Fragmenter`, for example
/// after the capture pipeline is rebuilt) a long-lived `Reassembler` drops every
/// frame as late until the new counter passes the old one.
///
/// Run with `cargo test -p v6cast-protocol -- --ignored sender_restart`.
#[test]
#[ignore = "known issue: Reassembler has no resync after the sender's frame_id restarts"]
fn sender_restart_is_picked_up_quickly() {
    let now = Instant::now();
    let mut r = Reassembler::default();
    let mut before = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    for _ in 0..1000 {
        for d in fragments(&mut before, &[1u8; 500], MediaFlags::keyframe(), 0) {
            r.push_datagram(&d, now);
        }
    }
    let mut after = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
    let mut delivered_within_a_second = 0;
    for _ in 0..60 {
        for d in fragments(&mut after, &[2u8; 500], MediaFlags::keyframe(), 0) {
            if matches!(r.push_datagram(&d, now), Push::Complete(_)) {
                delivered_within_a_second += 1;
            }
        }
    }
    assert!(
        delivered_within_a_second > 0,
        "60 frames (one second at 60 fps) after a sender restart, none was delivered"
    );
}
