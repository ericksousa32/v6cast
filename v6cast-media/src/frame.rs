//! Raw video frames passed between capture, scaler, codecs and renderer.

use thiserror::Error;

use crate::scale::Resolution;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FrameError {
    #[error("frame size {0} must be non-zero and even in both dimensions")]
    BadSize(Resolution),
    #[error("plane {plane} needs {needed} bytes, got {got}")]
    ShortPlane {
        plane: &'static str,
        needed: usize,
        got: usize,
    },
    #[error("plane {plane} stride {stride} is below its width {width}")]
    BadStride {
        plane: &'static str,
        stride: usize,
        width: usize,
    },
}

/// An I420 (planar YUV 4:2:0, BT.601 limited range) frame with tightly packed
/// planes: `width * height` luma bytes, then the U and V planes of
/// `width/2 * height/2` bytes each. Width and height are always even.
#[derive(Clone, PartialEq, Eq)]
pub struct YuvFrame {
    resolution: Resolution,
    data: Vec<u8>,
}

impl std::fmt::Debug for YuvFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YuvFrame")
            .field("resolution", &self.resolution)
            .finish_non_exhaustive()
    }
}

fn check_size(r: Resolution) -> Result<(), FrameError> {
    if r.width == 0 || r.height == 0 || !r.width.is_multiple_of(2) || !r.height.is_multiple_of(2) {
        return Err(FrameError::BadSize(r));
    }
    Ok(())
}

fn luma_len(r: Resolution) -> usize {
    r.width as usize * r.height as usize
}

fn chroma_len(r: Resolution) -> usize {
    luma_len(r) / 4
}

impl YuvFrame {
    /// A black frame.
    pub fn black(resolution: Resolution) -> Result<Self, FrameError> {
        check_size(resolution)?;
        let mut data = vec![16u8; luma_len(resolution)];
        data.resize(luma_len(resolution) + 2 * chroma_len(resolution), 128);
        Ok(Self { resolution, data })
    }

    /// Wraps packed I420 bytes.
    pub fn from_i420(resolution: Resolution, data: Vec<u8>) -> Result<Self, FrameError> {
        check_size(resolution)?;
        let needed = luma_len(resolution) + 2 * chroma_len(resolution);
        if data.len() != needed {
            return Err(FrameError::ShortPlane {
                plane: "i420",
                needed,
                got: data.len(),
            });
        }
        Ok(Self { resolution, data })
    }

    /// Copies planes that may have padded rows (strides) into a packed frame.
    pub fn from_strided(
        resolution: Resolution,
        planes: [(&[u8], usize); 3],
    ) -> Result<Self, FrameError> {
        check_size(resolution)?;
        let w = resolution.width as usize;
        let h = resolution.height as usize;
        let mut data = Vec::with_capacity(luma_len(resolution) + 2 * chroma_len(resolution));
        for ((plane, stride), (name, pw, ph)) in
            planes
                .into_iter()
                .zip([("y", w, h), ("u", w / 2, h / 2), ("v", w / 2, h / 2)])
        {
            if stride < pw {
                return Err(FrameError::BadStride {
                    plane: name,
                    stride,
                    width: pw,
                });
            }
            let needed = stride * (ph - 1) + pw;
            if plane.len() < needed {
                return Err(FrameError::ShortPlane {
                    plane: name,
                    needed,
                    got: plane.len(),
                });
            }
            for row in 0..ph {
                data.extend_from_slice(&plane[row * stride..row * stride + pw]);
            }
        }
        Ok(Self { resolution, data })
    }

    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    pub fn y(&self) -> &[u8] {
        &self.data[..luma_len(self.resolution)]
    }

    pub fn u(&self) -> &[u8] {
        let start = luma_len(self.resolution);
        &self.data[start..start + chroma_len(self.resolution)]
    }

    pub fn v(&self) -> &[u8] {
        let start = luma_len(self.resolution) + chroma_len(self.resolution);
        &self.data[start..]
    }

    /// Mutable `(y, u, v)` planes.
    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let l = luma_len(self.resolution);
        let c = chroma_len(self.resolution);
        let (y, rest) = self.data.split_at_mut(l);
        let (u, v) = rest.split_at_mut(c);
        (y, u, v)
    }

    /// The packed I420 bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_sizes_and_black() {
        let f = YuvFrame::black(Resolution::new(4, 2)).unwrap();
        assert_eq!((f.y().len(), f.u().len(), f.v().len()), (8, 2, 2));
        assert!(f.y().iter().all(|&b| b == 16));
        assert!(f.u().iter().chain(f.v()).all(|&b| b == 128));
    }

    #[test]
    fn rejects_odd_zero_and_short_input() {
        for r in [
            Resolution::new(0, 2),
            Resolution::new(3, 2),
            Resolution::new(2, 5),
        ] {
            assert_eq!(YuvFrame::black(r), Err(FrameError::BadSize(r)));
        }
        assert!(YuvFrame::from_i420(Resolution::new(2, 2), vec![0; 5]).is_err());
        assert!(YuvFrame::from_i420(Resolution::new(2, 2), vec![0; 6]).is_ok());
    }

    #[test]
    fn strided_planes_are_packed() {
        // 4x2 frame, luma stride 6, chroma stride 3.
        let y = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        let u = [9, 10, 0];
        let v = [11, 12, 0];
        let f = YuvFrame::from_strided(Resolution::new(4, 2), [(&y, 6), (&u, 3), (&v, 3)]).unwrap();
        assert_eq!(f.y(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(f.u(), &[9, 10]);
        assert_eq!(f.v(), &[11, 12]);
        assert!(
            YuvFrame::from_strided(Resolution::new(4, 2), [(&y, 3), (&u, 3), (&v, 3)]).is_err()
        );
        assert!(
            YuvFrame::from_strided(Resolution::new(4, 2), [(&y[..9], 6), (&u, 3), (&v, 3)])
                .is_err()
        );
    }
}
