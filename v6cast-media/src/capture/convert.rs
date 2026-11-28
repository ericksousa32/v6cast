//! Captured RGB pixels to I420, scaled to the encoder size in the same pass.

use super::CaptureError;
use crate::frame::YuvFrame;
use crate::scale::Resolution;

/// Byte order of a 4-byte-per-pixel capture format. The fourth byte (alpha or
/// padding) is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    Bgrx,
    Bgra,
    Rgbx,
    Rgba,
}

impl PixelLayout {
    /// Byte offsets of red, green and blue within a pixel.
    fn rgb(self) -> (usize, usize, usize) {
        match self {
            PixelLayout::Bgrx | PixelLayout::Bgra => (2, 1, 0),
            PixelLayout::Rgbx | PixelLayout::Rgba => (0, 1, 2),
        }
    }
}

/// A captured image as it sits in the PipeWire buffer.
#[derive(Debug, Clone, Copy)]
pub struct RawImage<'a> {
    pub data: &'a [u8],
    /// Bytes per row; at least `size.width * 4`.
    pub stride: usize,
    pub size: Resolution,
    pub layout: PixelLayout,
}

/// Source pixel range `[start, end)` covered by target pixel `t` of `target`.
fn footprint(t: usize, source: usize, target: usize) -> (usize, usize) {
    let start = t * source / target;
    let end = ((t + 1) * source / target).max(start + 1).min(source);
    (start, end)
}

/// Converts `image` to a BT.601 limited-range I420 frame of `target` size.
///
/// Each target pixel is the average of the source pixels it covers (a box
/// filter), so downscaling does not alias; upscaling repeats pixels. Chroma
/// is the average of each 2x2 block. `target` must be non-zero and even.
pub fn to_i420(image: &RawImage<'_>, target: Resolution) -> Result<YuvFrame, CaptureError> {
    let (sw, sh) = (image.size.width as usize, image.size.height as usize);
    if sw == 0 || sh == 0 {
        return Err(CaptureError::BadFrame("empty image"));
    }
    if image.stride < sw * 4 {
        return Err(CaptureError::BadFrame("stride shorter than a row"));
    }
    if image.data.len() < image.stride * (sh - 1) + sw * 4 {
        return Err(CaptureError::BadFrame("buffer shorter than the image"));
    }
    let mut out = YuvFrame::black(target)?;
    let (tw, th) = (target.width as usize, target.height as usize);
    let xs: Vec<(usize, usize)> = (0..tw).map(|x| footprint(x, sw, tw)).collect();
    let (ro, go, bo) = image.layout.rgb();
    let mut rows = [vec![[0i32; 3]; tw], vec![[0i32; 3]; tw]];
    let (y_plane, u_plane, v_plane) = out.planes_mut();
    for ty in (0..th).step_by(2) {
        for (half, row) in rows.iter_mut().enumerate() {
            let (y0, y1) = footprint(ty + half, sh, th);
            for (tx, &(x0, x1)) in xs.iter().enumerate() {
                let mut sum = [0u32; 3];
                for sy in y0..y1 {
                    let line = &image.data[sy * image.stride..];
                    for px in line[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                        sum[0] += u32::from(px[ro]);
                        sum[1] += u32::from(px[go]);
                        sum[2] += u32::from(px[bo]);
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                row[tx] = [
                    (sum[0] / n) as i32,
                    (sum[1] / n) as i32,
                    (sum[2] / n) as i32,
                ];
                let [r, g, b] = row[tx];
                y_plane[(ty + half) * tw + tx] =
                    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
            }
        }
        for cx in 0..tw / 2 {
            let mut rgb = [0i32; 3];
            for row in &rows {
                for p in &row[cx * 2..cx * 2 + 2] {
                    for c in 0..3 {
                        rgb[c] += p[c];
                    }
                }
            }
            let [r, g, b] = rgb.map(|v| v / 4);
            let i = (ty / 2) * (tw / 2) + cx;
            u_plane[i] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            v_plane[i] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(
        w: u32,
        h: u32,
        stride: usize,
        layout: PixelLayout,
        px: impl Fn(u32, u32) -> [u8; 3],
    ) -> Vec<u8> {
        let (ro, go, bo) = layout.rgb();
        let mut data = vec![0u8; stride * h as usize];
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = px(x, y);
                let i = y as usize * stride + x as usize * 4;
                data[i + ro] = r;
                data[i + go] = g;
                data[i + bo] = b;
                data[i + 3] = 0xff;
            }
        }
        data
    }

    fn convert(
        data: &[u8],
        size: (u32, u32),
        stride: usize,
        layout: PixelLayout,
        target: (u32, u32),
    ) -> YuvFrame {
        to_i420(
            &RawImage {
                data,
                stride,
                size: Resolution::new(size.0, size.1),
                layout,
            },
            Resolution::new(target.0, target.1),
        )
        .unwrap()
    }

    #[test]
    fn primary_colors_in_both_byte_orders() {
        let cases = [
            ([255, 255, 255], (235, 128, 128)),
            ([0, 0, 0], (16, 128, 128)),
            ([255, 0, 0], (82, 90, 240)),
            ([0, 255, 0], (144, 54, 34)), // integer BT.601: 16 + (129 * 255 + 128) >> 8
            ([0, 0, 255], (41, 240, 110)),
        ];
        for layout in [
            PixelLayout::Bgrx,
            PixelLayout::Bgra,
            PixelLayout::Rgbx,
            PixelLayout::Rgba,
        ] {
            for (rgb, (y, u, v)) in cases {
                let data = image(4, 4, 16, layout, |_, _| rgb);
                let f = convert(&data, (4, 4), 16, layout, (4, 4));
                assert!(
                    f.y().iter().all(|&p| p == y),
                    "{layout:?} {rgb:?}: y {:?}",
                    f.y()
                );
                assert!(
                    f.u().iter().all(|&p| p == u),
                    "{layout:?} {rgb:?}: u {:?}",
                    f.u()
                );
                assert!(
                    f.v().iter().all(|&p| p == v),
                    "{layout:?} {rgb:?}: v {:?}",
                    f.v()
                );
            }
        }
    }

    #[test]
    fn padded_rows_are_skipped() {
        // Stride larger than the row; padding bytes are garbage.
        let mut data = image(6, 2, 40, PixelLayout::Bgrx, |_, _| [255, 255, 255]);
        for y in 0..2 {
            data[y * 40 + 24..y * 40 + 40].fill(0x55);
        }
        let f = convert(&data, (6, 2), 40, PixelLayout::Bgrx, (6, 2));
        assert!(f.y().iter().all(|&p| p == 235));
    }

    #[test]
    fn downscaling_averages_the_footprint() {
        // 4x2 checkerboard of black and white, scaled to 2x2: each output pixel
        // covers one black and one white source pixel -> mid grey.
        let data = image(4, 2, 16, PixelLayout::Rgbx, |x, _| {
            if x % 2 == 0 {
                [0; 3]
            } else {
                [255; 3]
            }
        });
        let f = convert(&data, (4, 2), 16, PixelLayout::Rgbx, (2, 2));
        assert!(
            f.y().iter().all(|&p| (124..=127).contains(&p)),
            "{:?}",
            f.y()
        );
        // Left half white, right half black, 8x2 -> 4x2 keeps the edge.
        let data = image(8, 2, 32, PixelLayout::Rgbx, |x, _| {
            if x < 4 {
                [255; 3]
            } else {
                [0; 3]
            }
        });
        let f = convert(&data, (8, 2), 32, PixelLayout::Rgbx, (4, 2));
        assert_eq!(&f.y()[..4], &[235, 235, 16, 16]);
    }

    #[test]
    fn odd_sources_and_upscaling_work() {
        let data = image(5, 3, 20, PixelLayout::Bgrx, |_, _| [0, 0, 255]);
        let f = convert(&data, (5, 3), 20, PixelLayout::Bgrx, (4, 2));
        assert_eq!(f.resolution(), Resolution::new(4, 2));
        let f = convert(&data, (5, 3), 20, PixelLayout::Bgrx, (10, 6));
        assert!(f.y().iter().all(|&p| p == 41));
    }

    #[test]
    fn bad_buffers_are_errors() {
        let ok = vec![0u8; 16 * 4];
        let raw = |data: &[u8], stride, w, h| {
            to_i420(
                &RawImage {
                    data,
                    stride,
                    size: Resolution::new(w, h),
                    layout: PixelLayout::Bgrx,
                },
                Resolution::new(2, 2),
            )
        };
        assert!(raw(&ok, 16, 4, 4).is_ok());
        assert!(raw(&ok, 8, 4, 4).is_err()); // stride < row
        assert!(raw(&ok[..60], 16, 4, 4).is_err()); // too short
        assert!(raw(&ok, 16, 0, 4).is_err());
        let r = to_i420(
            &RawImage {
                data: &ok,
                stride: 16,
                size: Resolution::new(4, 4),
                layout: PixelLayout::Bgrx,
            },
            Resolution::new(3, 2),
        );
        assert!(r.is_err(), "odd target");
    }
}
