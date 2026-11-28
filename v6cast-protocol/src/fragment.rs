//! Splitting encoded frames into datagrams and putting them back together.
//!
//! # Datagram size
//!
//! IPv6 guarantees an MTU of 1280 bytes. Minus the IPv6 (40) and UDP (8) headers
//! that leaves 1232 bytes; quinn starts with a 1200-byte UDP payload. A QUIC short
//! header packet then spends at most about 44 bytes on the header, packet number,
//! AEAD tag and DATAGRAM frame header. [`IPV6_SAFE_DATAGRAM`] (1150 bytes) fits in
//! that budget on any IPv6 path without PMTU discovery. Callers can lower it to
//! what the QUIC connection reports.
//!
//! # Fragment layout
//!
//! A frame of `L` bytes sent as `n` fragments is cut in equal chunks of
//! `ceil(L / n)` bytes; the last one takes the rest. The receiver derives each
//! fragment's offset from `(L, n, index)`, so the header needs no offset field and
//! a fragment whose length does not match is rejected.
//!
//! # Reassembly policy
//!
//! Real-time media favours the newest frame. Per media kind the [`Reassembler`]:
//!
//! * accepts fragments in any order and ignores duplicates;
//! * delivers frames in increasing `frame_id` order: once a frame is delivered,
//!   fragments of it or of any older frame are dropped as late, and older
//!   incomplete frames are discarded;
//! * keeps at most [`ReassemblyConfig::max_partial_frames`] incomplete frames and
//!   [`ReassemblyConfig::max_buffered_bytes`] bytes in total, evicting the oldest
//!   incomplete frame first, and drops frames older than
//!   [`ReassemblyConfig::max_frame_age`];
//! * resynchronises when a fragment is more than [`RESYNC_DISTANCE`] frames behind
//!   the last delivered one: that only happens when the sender restarted its frame
//!   counter (a new `Fragmenter`), because genuinely late fragments are at most
//!   `max_frame_age` old. [`Reassembler::reset`] does the same on request.
//!
//! A restart within the first [`RESYNC_DISTANCE`] frames of a stream is not
//! detected: its frames are dropped as late until the new counter passes the old
//! one, at most [`RESYNC_DISTANCE`] frames (about one second at 60 fps).

use std::time::{Duration, Instant};

use crate::error::{invalid, Result};
use crate::media::{MediaFlags, MediaHeader, MediaKind, MAX_FRAME_LEN, MEDIA_HEADER_LEN};

/// Largest datagram (header and payload) that fits a QUIC packet on any IPv6 path.
pub const IPV6_SAFE_DATAGRAM: usize = 1150;

/// Smallest datagram size accepted by [`Fragmenter`]. It keeps the fragment
/// count of a [`MAX_FRAME_LEN`] frame within `u16`.
pub const MIN_DATAGRAM: usize = MEDIA_HEADER_LEN + 256;

/// Offset and length of fragment `index` in a frame of `frame_len` bytes cut in
/// `count` fragments. `None` when the combination is impossible.
pub fn fragment_span(frame_len: u32, count: u16, index: u16) -> Option<(usize, usize)> {
    if count == 0 || index >= count || frame_len == 0 || u32::from(count) > frame_len {
        return None;
    }
    let len = frame_len as usize;
    let count = usize::from(count);
    let chunk = len.div_ceil(count);
    let offset = usize::from(index) * chunk;
    if offset >= len {
        return None;
    }
    Some((offset, chunk.min(len - offset)))
}

/// A fragment this many frames (or more) behind the last delivered frame means
/// the sender restarted its counter; see the module docs.
pub const RESYNC_DISTANCE: u32 = 64;

/// `a` is newer than `b` in wrapping (serial number) order.
fn is_newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}

/// Cuts frames of one media kind into datagrams and numbers them.
#[derive(Debug)]
pub struct Fragmenter {
    kind: MediaKind,
    max_datagram: usize,
    next_frame_id: u32,
    next_seq: u32,
}

impl Fragmenter {
    /// `max_datagram` is the largest datagram, header included.
    pub fn new(kind: MediaKind, max_datagram: usize) -> Result<Self> {
        let mut f = Self {
            kind,
            max_datagram: MIN_DATAGRAM,
            next_frame_id: 0,
            next_seq: 0,
        };
        f.set_max_datagram(max_datagram)?;
        Ok(f)
    }

    /// Changes the datagram size, for example after the QUIC path MTU grew.
    pub fn set_max_datagram(&mut self, max_datagram: usize) -> Result<()> {
        if max_datagram < MIN_DATAGRAM {
            return Err(invalid("max_datagram", "below MIN_DATAGRAM"));
        }
        self.max_datagram = max_datagram;
        Ok(())
    }

    pub fn max_datagram(&self) -> usize {
        self.max_datagram
    }

    /// Splits one encoded frame into ready-to-send datagrams.
    pub fn fragment(
        &mut self,
        frame: &[u8],
        flags: MediaFlags,
        timestamp_us: u64,
    ) -> Result<Vec<Vec<u8>>> {
        if frame.is_empty() {
            return Err(invalid("frame", "empty"));
        }
        if frame.len() > MAX_FRAME_LEN as usize {
            return Err(invalid("frame", "larger than MAX_FRAME_LEN"));
        }
        let capacity = self.max_datagram - MEDIA_HEADER_LEN;
        let count = u16::try_from(frame.len().div_ceil(capacity))
            .map_err(|_| invalid("frame", "needs more than 65535 fragments"))?;
        let frame_len = frame.len() as u32;
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        let mut out = Vec::with_capacity(usize::from(count));
        for index in 0..count {
            let (offset, len) = fragment_span(frame_len, count, index)
                .ok_or(invalid("frame", "internal fragment layout error"))?;
            let header = MediaHeader {
                kind: self.kind,
                flags,
                seq: self.next_seq,
                frame_id,
                fragment_index: index,
                fragment_count: count,
                frame_len,
                timestamp_us,
            };
            self.next_seq = self.next_seq.wrapping_add(1);
            out.push(header.encode_datagram(&frame[offset..offset + len]));
        }
        Ok(out)
    }
}

/// A frame rebuilt from all its fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: MediaKind,
    pub flags: MediaFlags,
    pub frame_id: u32,
    pub timestamp_us: u64,
    pub data: Vec<u8>,
}

/// Why a fragment was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The frame, or a newer one, was already delivered.
    Late,
    /// The fragment was already received.
    Duplicate,
    /// Header fields or payload length do not match the frame.
    Invalid,
    /// The frame alone is larger than the memory budget.
    TooLarge,
}

/// Result of feeding one fragment to the [`Reassembler`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Push {
    Complete(Frame),
    Pending,
    Dropped(DropReason),
}

/// Limits of the [`Reassembler`].
#[derive(Debug, Clone, Copy)]
pub struct ReassemblyConfig {
    pub max_partial_frames: usize,
    pub max_buffered_bytes: usize,
    pub max_frame_age: Duration,
}

impl Default for ReassemblyConfig {
    fn default() -> Self {
        Self {
            max_partial_frames: 8,
            max_buffered_bytes: 32 * 1024 * 1024,
            max_frame_age: Duration::from_millis(500),
        }
    }
}

/// Counters for telemetry and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub frames_completed: u64,
    /// Incomplete frames discarded (evicted, expired or overtaken by a newer frame).
    pub frames_dropped: u64,
    pub fragments_late: u64,
    pub fragments_duplicate: u64,
    pub fragments_invalid: u64,
    pub fragments_too_large: u64,
    /// Times a sender restart was detected and the kind's state was cleared.
    pub resyncs: u64,
}

#[derive(Debug)]
struct Partial {
    kind: MediaKind,
    flags: MediaFlags,
    frame_id: u32,
    timestamp_us: u64,
    frame_len: u32,
    fragment_count: u16,
    received: Vec<bool>,
    received_count: u16,
    data: Vec<u8>,
    started: Instant,
}

impl Partial {
    fn matches(&self, h: &MediaHeader) -> bool {
        self.frame_len == h.frame_len
            && self.fragment_count == h.fragment_count
            && self.timestamp_us == h.timestamp_us
            && self.flags == h.flags
    }

    fn into_frame(self) -> Frame {
        Frame {
            kind: self.kind,
            flags: self.flags,
            frame_id: self.frame_id,
            timestamp_us: self.timestamp_us,
            data: self.data,
        }
    }
}

/// Rebuilds frames from fragments with bounded memory. See the module docs.
#[derive(Debug)]
pub struct Reassembler {
    config: ReassemblyConfig,
    partials: Vec<Partial>,
    buffered: usize,
    last_delivered: [Option<u32>; MediaKind::COUNT],
    stats: ReassemblyStats,
}

impl Reassembler {
    pub fn new(config: ReassemblyConfig) -> Self {
        Self {
            config,
            partials: Vec::new(),
            buffered: 0,
            last_delivered: [None; MediaKind::COUNT],
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    /// Bytes held by incomplete frames.
    pub fn buffered_bytes(&self) -> usize {
        self.buffered
    }

    pub fn partial_frames(&self) -> usize {
        self.partials.len()
    }

    /// Forgets everything about `kind`: the next frame is accepted whatever its
    /// id. Use it when the sender is known to have restarted its stream.
    /// Incomplete frames of `kind` count as dropped.
    pub fn reset(&mut self, kind: MediaKind) {
        self.last_delivered[kind.index()] = None;
        self.retain(|p| p.kind != kind);
    }

    /// Last frame delivered for `kind`.
    pub fn last_delivered(&self, kind: MediaKind) -> Option<u32> {
        self.last_delivered[kind.index()]
    }

    /// Decodes a raw datagram and feeds it. Malformed datagrams count as invalid.
    pub fn push_datagram(&mut self, datagram: &[u8], now: Instant) -> Push {
        match crate::media::decode_datagram(datagram) {
            Ok((header, payload)) => self.push(&header, payload, now),
            Err(_) => self.drop_fragment(DropReason::Invalid),
        }
    }

    /// Feeds one fragment.
    pub fn push(&mut self, header: &MediaHeader, payload: &[u8], now: Instant) -> Push {
        if header.validate().is_err() {
            return self.drop_fragment(DropReason::Invalid);
        }
        let Some((offset, len)) = fragment_span(
            header.frame_len,
            header.fragment_count,
            header.fragment_index,
        ) else {
            return self.drop_fragment(DropReason::Invalid);
        };
        if payload.len() != len {
            return self.drop_fragment(DropReason::Invalid);
        }
        if let Some(last) = self.last_delivered[header.kind.index()] {
            if !is_newer(header.frame_id, last) {
                if last.wrapping_sub(header.frame_id) < RESYNC_DISTANCE {
                    return self.drop_fragment(DropReason::Late);
                }
                // Far behind: the sender restarted its frame counter.
                self.stats.resyncs += 1;
                self.reset(header.kind);
            }
        }
        self.expire(now);

        let pos = match self
            .partials
            .iter()
            .position(|p| p.kind == header.kind && p.frame_id == header.frame_id)
        {
            Some(pos) => {
                if !self.partials[pos].matches(header) {
                    return self.drop_fragment(DropReason::Invalid);
                }
                pos
            }
            None => {
                let frame_len = header.frame_len as usize;
                if frame_len > self.config.max_buffered_bytes {
                    return self.drop_fragment(DropReason::TooLarge);
                }
                while !self.partials.is_empty()
                    && (self.partials.len() >= self.config.max_partial_frames
                        || self.buffered + frame_len > self.config.max_buffered_bytes)
                {
                    self.evict_oldest();
                }
                if self.config.max_partial_frames == 0 {
                    return self.drop_fragment(DropReason::TooLarge);
                }
                self.buffered += frame_len;
                self.partials.push(Partial {
                    kind: header.kind,
                    flags: header.flags,
                    frame_id: header.frame_id,
                    timestamp_us: header.timestamp_us,
                    frame_len: header.frame_len,
                    fragment_count: header.fragment_count,
                    received: vec![false; usize::from(header.fragment_count)],
                    received_count: 0,
                    data: vec![0; frame_len],
                    started: now,
                });
                self.partials.len() - 1
            }
        };

        let partial = &mut self.partials[pos];
        let index = usize::from(header.fragment_index);
        if partial.received[index] {
            return self.drop_fragment(DropReason::Duplicate);
        }
        partial.received[index] = true;
        partial.received_count += 1;
        partial.data[offset..offset + len].copy_from_slice(payload);
        if partial.received_count < partial.fragment_count {
            return Push::Pending;
        }

        let partial = self.partials.swap_remove(pos);
        self.buffered -= partial.data.len();
        let kind = partial.kind;
        let frame_id = partial.frame_id;
        self.last_delivered[kind.index()] = Some(frame_id);
        self.stats.frames_completed += 1;
        // Older incomplete frames of this kind can no longer be delivered in order.
        self.retain(|p| p.kind != kind || is_newer(p.frame_id, frame_id));
        Push::Complete(partial.into_frame())
    }

    /// Drops incomplete frames older than `max_frame_age`.
    pub fn expire(&mut self, now: Instant) {
        let max_age = self.config.max_frame_age;
        self.retain(|p| now.saturating_duration_since(p.started) <= max_age);
    }

    fn retain(&mut self, keep: impl Fn(&Partial) -> bool) {
        let before = self.partials.len();
        let mut freed = 0;
        self.partials.retain(|p| {
            let k = keep(p);
            if !k {
                freed += p.data.len();
            }
            k
        });
        self.buffered -= freed;
        self.stats.frames_dropped += (before - self.partials.len()) as u64;
    }

    fn evict_oldest(&mut self) {
        if let Some(pos) = self
            .partials
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| p.started)
            .map(|(i, _)| i)
        {
            let p = self.partials.swap_remove(pos);
            self.buffered -= p.data.len();
            self.stats.frames_dropped += 1;
        }
    }

    fn drop_fragment(&mut self, reason: DropReason) -> Push {
        let counter = match reason {
            DropReason::Late => &mut self.stats.fragments_late,
            DropReason::Duplicate => &mut self.stats.fragments_duplicate,
            DropReason::Invalid => &mut self.stats.fragments_invalid,
            DropReason::TooLarge => &mut self.stats.fragments_too_large,
        };
        *counter += 1;
        Push::Dropped(reason)
    }
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new(ReassemblyConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::decode_datagram;

    fn frame(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect()
    }

    fn feed(r: &mut Reassembler, datagrams: &[Vec<u8>], now: Instant) -> Vec<Push> {
        datagrams.iter().map(|d| r.push_datagram(d, now)).collect()
    }

    fn completed(pushes: &[Push]) -> Vec<&Frame> {
        pushes
            .iter()
            .filter_map(|p| match p {
                Push::Complete(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn spans_cover_the_frame_exactly() {
        // Every layout the sender produces: count = ceil(len / capacity).
        for len in 1..600u32 {
            for capacity in 1..=len {
                let count = len.div_ceil(capacity) as u16;
                let mut next = 0;
                for i in 0..count {
                    let (off, l) = fragment_span(len, count, i).unwrap();
                    assert_eq!(off, next);
                    assert!(l > 0);
                    next = off + l;
                }
                assert_eq!(next, len as usize, "len {len} count {count}");
            }
        }
        // Layouts no sender produces are refused instead of overlapping.
        assert_eq!(fragment_span(4, 3, 2), None);
        assert_eq!(fragment_span(10, 0, 0), None);
        assert_eq!(fragment_span(10, 2, 2), None);
        assert_eq!(fragment_span(3, 4, 0), None);
    }

    #[test]
    fn fragmenter_respects_datagram_size() {
        let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
        let cap = IPV6_SAFE_DATAGRAM - MEDIA_HEADER_LEN;
        for len in [1, cap - 1, cap, cap + 1, 3 * cap, 200_000] {
            let data = frame(len, 1);
            let out = f.fragment(&data, MediaFlags::default(), 0).unwrap();
            assert_eq!(out.len(), len.div_ceil(cap));
            assert!(out.iter().all(|d| d.len() <= IPV6_SAFE_DATAGRAM));
        }
    }

    #[test]
    fn fragmenter_rejects_bad_input() {
        assert!(Fragmenter::new(MediaKind::Video, MIN_DATAGRAM - 1).is_err());
        let mut f = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        assert!(f.fragment(&[], MediaFlags::default(), 0).is_err());
        let big = vec![0u8; MAX_FRAME_LEN as usize + 1];
        assert!(f.fragment(&big, MediaFlags::default(), 0).is_err());
        // The largest frame still fits u16 fragments at the smallest datagram.
        let max = vec![0u8; MAX_FRAME_LEN as usize];
        assert!(f.fragment(&max, MediaFlags::default(), 0).is_ok());
    }

    #[test]
    fn sequence_and_frame_ids_advance_and_wrap() {
        let mut f = Fragmenter::new(MediaKind::Audio, MIN_DATAGRAM).unwrap();
        f.next_seq = u32::MAX;
        f.next_frame_id = u32::MAX;
        let a = f
            .fragment(&frame(600, 0), MediaFlags::default(), 0)
            .unwrap();
        let b = f.fragment(&frame(10, 0), MediaFlags::default(), 0).unwrap();
        let seqs: Vec<u32> = a
            .iter()
            .chain(&b)
            .map(|d| decode_datagram(d).unwrap().0.seq)
            .collect();
        assert_eq!(seqs, vec![u32::MAX, 0, 1, 2]);
        assert_eq!(decode_datagram(&a[0]).unwrap().0.frame_id, u32::MAX);
        assert_eq!(decode_datagram(&b[0]).unwrap().0.frame_id, 0);
    }

    #[test]
    fn roundtrip_in_order_and_reversed() {
        let now = Instant::now();
        let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
        let mut r = Reassembler::default();
        let data = frame(10_000, 7);
        let d = f.fragment(&data, MediaFlags::keyframe(), 42).unwrap();
        let pushes = feed(&mut r, &d, now);
        let frames = completed(&pushes);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, data);
        assert!(frames[0].flags.is_keyframe());
        assert_eq!(frames[0].timestamp_us, 42);

        let data = frame(5_000, 9);
        let mut d = f.fragment(&data, MediaFlags::default(), 43).unwrap();
        d.reverse();
        let pushes = feed(&mut r, &d, now);
        assert_eq!(completed(&pushes)[0].data, data);
        assert_eq!(r.buffered_bytes(), 0);
        assert_eq!(r.partial_frames(), 0);
    }

    #[test]
    fn duplicates_and_late_fragments_are_dropped() {
        let now = Instant::now();
        let mut f = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        let mut r = Reassembler::default();
        let d = f
            .fragment(&frame(1000, 1), MediaFlags::default(), 0)
            .unwrap();
        assert_eq!(r.push_datagram(&d[0], now), Push::Pending);
        assert_eq!(
            r.push_datagram(&d[0], now),
            Push::Dropped(DropReason::Duplicate)
        );
        let rest = feed(&mut r, &d[1..], now);
        assert_eq!(completed(&rest).len(), 1);
        // Same frame again after delivery: late.
        assert_eq!(r.push_datagram(&d[1], now), Push::Dropped(DropReason::Late));
        let s = r.stats();
        assert_eq!((s.fragments_duplicate, s.fragments_late), (1, 1));
    }

    #[test]
    fn newer_frame_overtakes_an_incomplete_older_one() {
        let now = Instant::now();
        let mut f = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        let mut r = Reassembler::default();
        let old = f
            .fragment(&frame(1000, 1), MediaFlags::default(), 1)
            .unwrap();
        let new = f
            .fragment(&frame(1000, 2), MediaFlags::default(), 2)
            .unwrap();
        // Interleave, but hold back the last fragment of the old frame.
        let mut order = Vec::new();
        for i in 0..new.len() {
            if i + 1 < old.len() {
                order.push(old[i].clone());
            }
            order.push(new[i].clone());
        }
        let frames_out = feed(&mut r, &order, now);
        let frames = completed(&frames_out);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_id, 1);
        assert_eq!(r.stats().frames_dropped, 1);
        assert_eq!(r.partial_frames(), 0);
        // The missing old fragment arrives too late.
        assert_eq!(
            r.push_datagram(old.last().unwrap(), now),
            Push::Dropped(DropReason::Late)
        );
    }

    #[test]
    fn lost_fragment_never_completes_and_expires() {
        let start = Instant::now();
        let mut f = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        let mut r = Reassembler::default();
        let d = f
            .fragment(&frame(2000, 3), MediaFlags::default(), 0)
            .unwrap();
        let pushes = feed(&mut r, &d[1..], start);
        assert!(completed(&pushes).is_empty());
        assert!(r.buffered_bytes() > 0);
        r.expire(start + Duration::from_millis(501));
        assert_eq!(r.partial_frames(), 0);
        assert_eq!(r.buffered_bytes(), 0);
        assert_eq!(r.stats().frames_dropped, 1);
    }

    #[test]
    fn inconsistent_fragments_are_invalid() {
        let now = Instant::now();
        let mut f = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        let mut r = Reassembler::default();
        let d = f
            .fragment(&frame(1000, 1), MediaFlags::default(), 5)
            .unwrap();
        r.push_datagram(&d[0], now);
        let (mut h, payload) = decode_datagram(&d[1]).unwrap();
        h.timestamp_us = 6;
        assert_eq!(r.push(&h, payload, now), Push::Dropped(DropReason::Invalid));
        let (h, payload) = decode_datagram(&d[1]).unwrap();
        assert_eq!(
            r.push(&h, &payload[1..], now),
            Push::Dropped(DropReason::Invalid)
        );
        assert_eq!(
            r.push_datagram(b"garbage", now),
            Push::Dropped(DropReason::Invalid)
        );
        assert_eq!(r.stats().fragments_invalid, 3);
    }

    #[test]
    fn memory_stays_within_budget() {
        let now = Instant::now();
        let config = ReassemblyConfig {
            max_partial_frames: 4,
            max_buffered_bytes: 10_000,
            ..ReassemblyConfig::default()
        };
        let mut r = Reassembler::new(config);
        // Headers announcing large frames, only the first fragment of each arrives.
        for id in 0..50u32 {
            let h = MediaHeader {
                kind: MediaKind::Video,
                flags: MediaFlags::default(),
                seq: id,
                frame_id: id,
                fragment_index: 0,
                fragment_count: 4,
                frame_len: 4000,
                timestamp_us: 0,
            };
            let (_, len) = fragment_span(4000, 4, 0).unwrap();
            assert_eq!(r.push(&h, &vec![0; len], now), Push::Pending);
            assert!(r.buffered_bytes() <= config.max_buffered_bytes);
            assert!(r.partial_frames() <= config.max_partial_frames);
        }
        assert_eq!(r.stats().frames_dropped, 48);
        // A single frame larger than the budget is refused outright.
        let h = MediaHeader {
            kind: MediaKind::Video,
            flags: MediaFlags::default(),
            seq: 0,
            frame_id: 100,
            fragment_index: 0,
            fragment_count: 100,
            frame_len: 20_000,
            timestamp_us: 0,
        };
        assert_eq!(
            r.push(&h, &[0; 200], now),
            Push::Dropped(DropReason::TooLarge)
        );
    }

    #[test]
    fn audio_and_video_reassemble_independently() {
        let now = Instant::now();
        let mut v = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        let mut a = Fragmenter::new(MediaKind::Audio, MIN_DATAGRAM).unwrap();
        let mut r = Reassembler::default();
        let vd = v
            .fragment(&frame(900, 1), MediaFlags::default(), 0)
            .unwrap();
        let ad = a
            .fragment(&frame(300, 2), MediaFlags::default(), 0)
            .unwrap();
        // Both start at frame id 0; delivering audio must not mark video late.
        assert_eq!(completed(&feed(&mut r, &ad, now)).len(), 1);
        assert_eq!(completed(&feed(&mut r, &vd, now)).len(), 1);
        assert_eq!(r.last_delivered(MediaKind::Audio), Some(0));
        assert_eq!(r.last_delivered(MediaKind::Video), Some(0));
    }

    fn single(f: &mut Fragmenter, byte: u8) -> Vec<u8> {
        f.fragment(&[byte; 100], MediaFlags::keyframe(), 0)
            .unwrap()
            .remove(0)
    }

    #[test]
    fn sender_restart_far_behind_resyncs_at_once() {
        let now = Instant::now();
        let mut r = Reassembler::default();
        let mut old = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        for _ in 0..200 {
            r.push_datagram(&single(&mut old, 1), now);
        }
        assert_eq!(r.last_delivered(MediaKind::Video), Some(199));
        let mut new = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        assert!(matches!(
            r.push_datagram(&single(&mut new, 2), now),
            Push::Complete(Frame { frame_id: 0, .. })
        ));
        assert_eq!(r.stats().resyncs, 1);
        assert_eq!(r.last_delivered(MediaKind::Video), Some(0));
    }

    #[test]
    fn genuinely_late_fragments_do_not_resync() {
        let now = Instant::now();
        let mut r = Reassembler::default();
        let mut f = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        let frames: Vec<Vec<u8>> = (0..=RESYNC_DISTANCE).map(|_| single(&mut f, 1)).collect();
        r.push_datagram(frames.last().unwrap(), now);
        // RESYNC_DISTANCE - 1 frames behind: late.
        assert_eq!(
            r.push_datagram(&frames[1], now),
            Push::Dropped(DropReason::Late)
        );
        assert_eq!(r.stats().resyncs, 0);
        // RESYNC_DISTANCE frames behind: treated as a restart.
        assert!(matches!(
            r.push_datagram(&frames[0], now),
            Push::Complete(_)
        ));
        assert_eq!(r.stats().resyncs, 1);
    }

    #[test]
    fn reset_forgets_one_kind_only() {
        let now = Instant::now();
        let mut r = Reassembler::default();
        let mut v = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        let mut a = Fragmenter::new(MediaKind::Audio, MIN_DATAGRAM).unwrap();
        r.push_datagram(&single(&mut v, 1), now);
        r.push_datagram(&single(&mut a, 1), now);
        // An incomplete video frame is pending when the reset happens.
        let pending = v.fragment(&[3; 1000], MediaFlags::default(), 0).unwrap();
        r.push_datagram(&pending[0], now);
        r.reset(MediaKind::Video);
        assert_eq!(r.last_delivered(MediaKind::Video), None);
        assert_eq!(r.last_delivered(MediaKind::Audio), Some(0));
        assert_eq!(r.partial_frames(), 0);
        assert_eq!(r.buffered_bytes(), 0);
        assert_eq!(r.stats().frames_dropped, 1);
    }

    #[test]
    fn frame_ids_wrap_around() {
        let now = Instant::now();
        let mut f = Fragmenter::new(MediaKind::Video, MIN_DATAGRAM).unwrap();
        f.next_frame_id = u32::MAX - 1;
        let mut r = Reassembler::default();
        let mut ids = Vec::new();
        for _ in 0..4 {
            let d = f
                .fragment(&frame(700, 0), MediaFlags::default(), 0)
                .unwrap();
            ids.extend(completed(&feed(&mut r, &d, now)).iter().map(|f| f.frame_id));
        }
        assert_eq!(ids, vec![u32::MAX - 1, u32::MAX, 0, 1]);
    }

    #[test]
    fn shuffled_random_frames_roundtrip() {
        let now = Instant::now();
        let mut state = 0x1234_5678_9abc_def0_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut f = Fragmenter::new(MediaKind::Video, IPV6_SAFE_DATAGRAM).unwrap();
        let mut r = Reassembler::default();
        for round in 0..200 {
            let len = 1 + (next() % 40_000) as usize;
            let data = frame(len, round as u8);
            let mut d = f.fragment(&data, MediaFlags::default(), round).unwrap();
            for i in (1..d.len()).rev() {
                d.swap(i, (next() % (i as u64 + 1)) as usize);
            }
            let pushes = feed(&mut r, &d, now);
            assert_eq!(completed(&pushes)[0].data, data);
        }
        assert_eq!(r.stats().frames_completed, 200);
        assert_eq!(r.buffered_bytes(), 0);
    }
}
