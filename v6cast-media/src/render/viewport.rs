//! Where a frame goes inside a window: centered, aspect ratio kept.

use crate::scale::Resolution;

/// A rectangle in target pixels, origin at the top left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Largest rectangle with the frame's aspect ratio that fits `target`,
/// centered. The rest of the target stays black (letterbox or pillarbox).
/// Scales up as well as down, so small streams fill the window.
///
/// `None` when either size is zero. Sizes are rounded to the nearest pixel, so
/// the aspect ratio is off by at most half a pixel per side.
pub fn fit_viewport(frame: Resolution, target: Resolution) -> Option<Viewport> {
    if frame.width == 0 || frame.height == 0 || target.width == 0 || target.height == 0 {
        return None;
    }
    let (fw, fh) = (u64::from(frame.width), u64::from(frame.height));
    let (tw, th) = (u64::from(target.width), u64::from(target.height));
    // Compare tw/fw with th/fh without dividing.
    let (w, h) = if tw * fh <= th * fw {
        // Width-limited: full width, height = fh * tw / fw, rounded.
        (tw, (fh * tw + fw / 2) / fw)
    } else {
        ((fw * th + fh / 2) / fh, th)
    };
    // Rounding can never exceed the target, but a 1-pixel side could round to 0.
    let (w, h) = (w.clamp(1, tw), h.clamp(1, th));
    Some(Viewport {
        x: ((tw - w) / 2) as u32,
        y: ((th - h) / 2) as u32,
        width: w as u32,
        height: h as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(w: u32, h: u32) -> Resolution {
        Resolution::new(w, h)
    }

    fn vp(x: u32, y: u32, width: u32, height: u32) -> Viewport {
        Viewport {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn same_aspect_fills_the_window() {
        assert_eq!(
            fit_viewport(r(1920, 1080), r(1280, 720)),
            Some(vp(0, 0, 1280, 720))
        );
        assert_eq!(
            fit_viewport(r(1280, 720), r(2560, 1440)),
            Some(vp(0, 0, 2560, 1440))
        );
    }

    #[test]
    fn wider_frames_get_letterboxed() {
        // 21:9 stream in a 16:9 window.
        assert_eq!(
            fit_viewport(r(2560, 1080), r(1920, 1080)),
            Some(vp(0, 135, 1920, 810))
        );
        // 16:9 stream in a 4:3 window.
        assert_eq!(
            fit_viewport(r(1920, 1080), r(1024, 768)),
            Some(vp(0, 96, 1024, 576))
        );
    }

    #[test]
    fn taller_frames_get_pillarboxed() {
        // 16:10 stream in a 16:9 window.
        assert_eq!(
            fit_viewport(r(2304, 1440), r(1920, 1080)),
            Some(vp(96, 0, 1728, 1080))
        );
        // Portrait window capture in a landscape window.
        assert_eq!(
            fit_viewport(r(486, 1080), r(1920, 1080)),
            Some(vp(717, 0, 486, 1080))
        );
    }

    #[test]
    fn never_distorts_and_stays_centered() {
        for (fw, fh) in [
            (1920, 1080),
            (2560, 1080),
            (2304, 1440),
            (640, 1000),
            (7, 3),
        ] {
            for (tw, th) in [(800, 600), (1920, 1080), (333, 777), (3440, 1440), (1, 1)] {
                let v = fit_viewport(r(fw, fh), r(tw, th)).unwrap();
                assert!(v.x + v.width <= tw && v.y + v.height <= th);
                // One side touches the window edge.
                assert!(
                    v.width == tw || v.height == th,
                    "{fw}x{fh} in {tw}x{th}: {v:?}"
                );
                // Centered within one pixel.
                let (left, right) = (v.x, tw - v.x - v.width);
                let (top, bottom) = (v.y, th - v.y - v.height);
                assert!(left.abs_diff(right) <= 1 && top.abs_diff(bottom) <= 1);
                // Aspect error from rounding: at most half a pixel on the short side.
                let a = f64::from(fw) / f64::from(fh);
                let b = f64::from(v.width) / f64::from(v.height);
                let tolerance = a * 0.5 / f64::from(v.width.min(v.height)) + 1e-9;
                assert!(
                    (a - b).abs() / a <= 2.0 * tolerance + 1.0 / f64::from(v.height),
                    "{v:?}"
                );
            }
        }
    }

    #[test]
    fn zero_and_extreme_sizes_do_not_panic() {
        assert_eq!(fit_viewport(r(0, 1080), r(800, 600)), None);
        assert_eq!(fit_viewport(r(1920, 1080), r(0, 600)), None);
        assert_eq!(fit_viewport(r(1920, 1080), r(1, 1)), Some(vp(0, 0, 1, 1)));
        // A 10000:1 strip in a square still gets one row.
        assert_eq!(
            fit_viewport(r(10_000, 1), r(100, 100)),
            Some(vp(0, 49, 100, 1))
        );
        let huge = fit_viewport(r(u32::MAX, u32::MAX), r(u32::MAX, 1)).unwrap();
        assert_eq!((huge.width, huge.height), (1, 1));
    }
}
