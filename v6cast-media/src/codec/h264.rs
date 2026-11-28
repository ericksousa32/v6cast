//! Small H.264 bitstream helpers shared by the backends (pure, no I/O).

use crate::scale::Resolution;

/// NAL unit type of an IDR slice.
pub const NAL_IDR: u8 = 5;
/// NAL unit type of a sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// NAL unit type of a picture parameter set.
pub const NAL_PPS: u8 = 8;

/// NAL unit types in an Annex B buffer, in order. Accepts 3- and 4-byte start
/// codes and ignores bytes before the first start code.
pub fn nal_unit_types(annex_b: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 <= annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            if let Some(&header) = annex_b.get(i + 3) {
                types.push(header & 0x1f);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

/// True when the access unit contains an IDR slice.
pub fn is_idr(annex_b: &[u8]) -> bool {
    nal_unit_types(annex_b).contains(&NAL_IDR)
}

/// Reads unsigned Exp-Golomb values from the start of an RBSP, skipping
/// emulation prevention bytes. `None` when the data runs out.
struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u8,
    zeros: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte: 0,
            bit: 0,
            zeros: 0,
        }
    }

    fn bit(&mut self) -> Option<u32> {
        if self.bit == 0 {
            // 00 00 03 -> the 03 is an emulation prevention byte.
            if self.zeros >= 2 && self.data.get(self.byte) == Some(&3) {
                self.byte += 1;
                self.zeros = 0;
            }
            let b = *self.data.get(self.byte)?;
            self.zeros = if b == 0 { self.zeros + 1 } else { 0 };
        }
        let b = self.data[self.byte];
        let v = (b >> (7 - self.bit)) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.byte += 1;
        }
        Some(u32::from(v))
    }

    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return None;
            }
        }
        let mut v = 0u32;
        for _ in 0..leading {
            v = (v << 1) | self.bit()?;
        }
        Some((1u32 << leading) - 1 + v)
    }
}

/// Slice type of a slice NAL payload (after the one-byte NAL header):
/// `first_mb_in_slice` then `slice_type`, both Exp-Golomb. Values 5..=9 mean
/// "all slices of the picture have this type"; the result is folded to 0..=4
/// (0 P, 1 B, 2 I, 3 SP, 4 SI).
pub fn slice_type(payload: &[u8]) -> Option<u32> {
    let mut r = BitReader::new(payload);
    r.ue()?; // first_mb_in_slice
    r.ue().filter(|t| *t <= 9).map(|t| t % 5)
}

/// Repairs two defects of cros-codecs + Mesa VA-API H.264 output, in place.
///
/// 1. Mesa's encoder (radeonsi, 24.x and later) takes `nal_ref_idc` and
///    `nal_unit_type` from the application's packed slice header. cros-codecs
///    does not send one, so Mesa writes a zero slice NAL header byte: type 0,
///    which no valid stream contains. Every picture is a reference in our
///    low-delay streams. An I slice becomes an IDR slice (`0x65`) only when its
///    access unit carries an SPS, which cros-codecs writes in front of every IDR
///    and only there; every other slice, I or P, becomes a non-IDR reference
///    slice (`0x41`). The IDR slice header syntax itself comes from Mesa, which
///    knows from the picture parameters whether the picture is an IDR.
/// 2. The stream is encoded with VA's Constrained Baseline profile, but the
///    SPS carries `profile_idc` 66 with no constraint flags, which decoders
///    (cros-codecs among them) refuse as plain Baseline. `constraint_set0` and
///    `constraint_set1` are set, as Constrained Baseline requires. Setting bits
///    cannot create a start-code emulation.
///
/// Returns how many NAL units were changed.
pub fn repair_vaapi_stream(annex_b: &mut [u8]) -> usize {
    let starts_sequence = nal_unit_types(annex_b).contains(&NAL_SPS);
    let mut fixed = 0;
    let mut i = 0;
    while i + 4 <= annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            let h = i + 3;
            if annex_b[h] == 0 {
                annex_b[h] = match slice_type(&annex_b[h + 1..]) {
                    Some(2 | 4) if starts_sequence => 0x65,
                    _ => 0x41,
                };
                fixed += 1;
            } else if annex_b[h] & 0x1f == NAL_SPS
                && annex_b.get(h + 1) == Some(&66)
                && annex_b.get(h + 2).is_some_and(|c| c & 0xc0 != 0xc0)
            {
                annex_b[h + 2] |= 0xc0;
                fixed += 1;
            }
            i = h;
        } else {
            i += 1;
        }
    }
    fixed
}

/// H.264 level, as `level_idc` (10 times the level number).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(pub u8);

/// (level_idc, MaxMBPS, MaxFS) from H.264 Table A-1, levels 3.1 to 5.2.
const LEVELS: [(u8, u64, u64); 8] = [
    (31, 108_000, 3_600),
    (32, 216_000, 5_120),
    (40, 245_760, 8_192),
    (41, 245_760, 8_192),
    (42, 522_240, 8_704),
    (50, 589_824, 22_080),
    (51, 983_040, 36_864),
    (52, 2_073_600, 36_864),
];

/// Lowest level that allows `resolution` at `fps` frames per second.
/// Sizes beyond level 5.2 get 5.2 (the encoder may then refuse them).
pub fn level_for(resolution: Resolution, fps: u32) -> Level {
    let mbs = u64::from(resolution.width.div_ceil(16)) * u64::from(resolution.height.div_ceil(16));
    let mbps = mbs * u64::from(fps);
    LEVELS
        .iter()
        .find(|(_, max_mbps, max_fs)| mbs <= *max_fs && mbps <= *max_mbps)
        .map_or(Level(52), |(idc, _, _)| Level(*idc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nal_types_with_both_start_codes() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS, 4-byte start code
            0, 0, 1, 0x68, 3, // PPS, 3-byte start code
            0, 0, 1, 0x65, 9, 9, // IDR slice
        ];
        assert_eq!(nal_unit_types(&au), vec![NAL_SPS, NAL_PPS, NAL_IDR]);
        assert!(is_idr(&au));
        assert!(!is_idr(&[0, 0, 0, 1, 0x41, 7])); // non-IDR slice
        assert!(nal_unit_types(&[]).is_empty());
        assert!(nal_unit_types(&[0, 0, 1]).is_empty()); // start code, no header
        assert!(nal_unit_types(&[9, 9, 9, 9]).is_empty());
    }

    #[test]
    fn exp_golomb_slice_types() {
        // first_mb 0 ('1'), slice_type 7 ('0001000') -> I; then padding.
        assert_eq!(slice_type(&[0b1000_1000, 0x80]), Some(2));
        // first_mb 0, slice_type 5 ('00110') -> P.
        assert_eq!(slice_type(&[0b1001_1000]), Some(0));
        // Real Mesa output: 88 81.. is an I slice, 9a.. a P slice.
        assert_eq!(slice_type(&[0x88, 0x81, 0x00, 0x17]), Some(2));
        assert_eq!(slice_type(&[0x9a, 0x00, 0x00, 0x7f]), Some(0));
        assert_eq!(slice_type(&[]), None);
        assert_eq!(slice_type(&[0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn mesa_stream_defects_are_repaired() {
        // Bytes as Mesa radeonsi + cros-codecs produce them.
        let mut au = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x20, 0, 0, 0, 1, 0x68, 0xce];
        au.extend([0, 0, 0, 1, 0x00, 0x88, 0x81, 0x00]); // broken IDR slice
        assert_eq!(repair_vaapi_stream(&mut au), 2);
        assert_eq!(nal_unit_types(&au), vec![NAL_SPS, NAL_PPS, NAL_IDR]);
        assert_eq!(&au[4..8], &[0x67, 0x42, 0xc0, 0x20]);
        // An I slice without parameter sets is not an IDR.
        let mut i_slice = vec![0, 0, 0, 1, 0x00, 0x88, 0x81, 0x00];
        assert_eq!(repair_vaapi_stream(&mut i_slice), 1);
        assert_eq!(nal_unit_types(&i_slice), vec![1]);
        let mut p = vec![0, 0, 0, 0, 1, 0x00, 0x9a, 0x00, 0x00, 0x7f];
        assert_eq!(repair_vaapi_stream(&mut p), 1);
        assert_eq!(p[5], 0x41);
        // Valid streams, and Main profile SPS, are left alone.
        let mut ok = vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 1, 0x65, 0x88];
        let before = ok.clone();
        assert_eq!(repair_vaapi_stream(&mut ok), 0);
        assert_eq!(ok, before);
        let mut main = vec![0, 0, 0, 1, 0x67, 0x4d, 0x00, 0x28];
        assert_eq!(repair_vaapi_stream(&mut main), 0);
        assert_eq!(repair_vaapi_stream(&mut [0, 0, 1]), 0);
    }

    #[test]
    fn levels_for_the_presets() {
        let r = Resolution::new;
        assert_eq!(level_for(r(1280, 720), 30), Level(31));
        assert_eq!(level_for(r(1280, 720), 60), Level(32));
        assert_eq!(level_for(r(1920, 1080), 30), Level(40));
        assert_eq!(level_for(r(1920, 1080), 60), Level(42));
        assert_eq!(level_for(r(2560, 1440), 30), Level(50));
        assert_eq!(level_for(r(2560, 1440), 60), Level(51));
        assert_eq!(level_for(r(2304, 1440), 60), Level(51));
        assert_eq!(level_for(r(7680, 4320), 60), Level(52));
    }
}
