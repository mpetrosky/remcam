//! JPEG (camera MJPEG frames) to planar YUV.
//!
//! With the `turbojpeg` feature, libjpeg-turbo decodes straight to the
//! JPEG's own planar YUV, which is about 2x faster than the pure-Rust
//! `zune-jpeg` path: that one can only produce RGB, which then needs a full
//! color conversion. The `zune-jpeg` path is also used for chroma layouts
//! other than 4:2:2 and 4:2:0.

use anyhow::Context;
use zune_jpeg::JpegDecoder as ZuneDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;

use crate::Planes;

pub struct JpegDecoder {
    rgb: Vec<u8>,
    #[cfg(feature = "turbojpeg")]
    turbo: turbo::Turbo,
}

impl JpegDecoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            rgb: Vec::new(),
            #[cfg(feature = "turbojpeg")]
            turbo: turbo::Turbo::new()?,
        })
    }

    /// Decode `jpeg` into `out`, which takes the JPEG's size. Callers check
    /// that against the size they expect.
    pub fn decode(&mut self, jpeg: &[u8], out: &mut Planes) -> anyhow::Result<()> {
        #[cfg(feature = "turbojpeg")]
        if self.turbo.decode(jpeg, out)? {
            return Ok(());
        }
        self.decode_rgb(jpeg, out)
    }

    /// Fallback: decode to RGB and convert to limited-range 4:2:2.
    fn decode_rgb(&mut self, jpeg: &[u8], out: &mut Planes) -> anyhow::Result<()> {
        let mut decoder = ZuneDecoder::new(ZCursor::new(jpeg));
        decoder.decode_headers().context("bad JPEG header")?;
        let info = decoder.info().context("bad JPEG header")?;
        let (w, h) = (usize::from(info.width), usize::from(info.height));
        anyhow::ensure!(w.is_multiple_of(2), "JPEG width {w} is odd");
        self.rgb.resize(w * h * 3, 0);
        decoder
            .decode_into(&mut self.rgb)
            .context("decoding JPEG")?;
        out.reset(w, h, h, false);
        rgb_to_planes(&self.rgb, out);
        Ok(())
    }
}

/// Packed RGB to limited-range planar 4:2:2 using BT.601 coefficients.
/// Chroma is taken from the average of each horizontal pixel pair.
fn rgb_to_planes(rgb: &[u8], out: &mut Planes) {
    #[inline(always)]
    fn luma(r: i32, g: i32, b: i32) -> u8 {
        (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
    }
    let (y, u, v) = out.planes_mut();
    let pixels = rgb.as_chunks::<6>().0;
    for (((px, y), u), v) in pixels
        .iter()
        .zip(y.as_chunks_mut::<2>().0)
        .zip(u.iter_mut())
        .zip(v.iter_mut())
    {
        let [r0, g0, b0, r1, g1, b1] = px.map(i32::from);
        let (r, g, b) = ((r0 + r1) / 2, (g0 + g1) / 2, (b0 + b1) / 2);
        *y = [luma(r0, g0, b0), luma(r1, g1, b1)];
        *u = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
        *v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
    }
}

#[cfg(feature = "turbojpeg")]
mod turbo {
    use anyhow::Context;
    use tracing::debug;
    use turbojpeg::{Decompressor, Subsamp, YuvImage};

    use crate::Planes;

    pub struct Turbo {
        decompressor: Decompressor,
        warned: bool,
    }

    impl Turbo {
        pub fn new() -> anyhow::Result<Self> {
            Ok(Self {
                decompressor: Decompressor::new().context("initializing libjpeg-turbo")?,
                warned: false,
            })
        }

        /// Returns `Ok(false)` if the frame's chroma layout isn't handled
        /// here, so the caller can fall back to the RGB path.
        pub fn decode(&mut self, jpeg: &[u8], out: &mut Planes) -> anyhow::Result<bool> {
            let header = self
                .decompressor
                .read_header(jpeg)
                .context("bad JPEG header")?;
            let (w, h) = (header.width, header.height);
            // UVC cameras send 4:2:2; 4:2:0 shows up with some devices.
            let chroma_height = match header.subsamp {
                Subsamp::Sub2x1 => h,
                Subsamp::Sub2x2 => h.div_ceil(2),
                other => {
                    if !self.warned {
                        debug!("{other:?} JPEG, using the slower RGB path");
                        self.warned = true;
                    }
                    return Ok(false);
                }
            };
            anyhow::ensure!(w.is_multiple_of(2), "JPEG width {w} is odd");
            out.reset(w, h, chroma_height, true);
            let image = YuvImage {
                pixels: out.data_mut(),
                width: w,
                align: 1,
                height: h,
                subsamp: header.subsamp,
            };
            self.decompressor
                .decompress_to_yuv(jpeg, image)
                .context("decoding JPEG")?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yuyv(p: &Planes) -> Vec<u8> {
        let mut out = vec![0; p.width * p.height * 2];
        p.to_yuyv(&mut out);
        out
    }

    #[test]
    fn reference_colors() {
        let mut p = Planes::default();
        p.reset(2, 1, 1, false);
        // white, black -> Y 235/16, neutral chroma
        rgb_to_planes(&[255, 255, 255, 0, 0, 0], &mut p);
        assert_eq!(p.y(), &[235, 16]);
        assert!((127..=129).contains(&p.u()[0]) && (127..=129).contains(&p.v()[0]));

        // pure red (BT.601: Y=82, U=90, V=240)
        rgb_to_planes(&[255, 0, 0, 255, 0, 0], &mut p);
        assert_eq!(yuyv(&p), [82, 90, 82, 240]);
    }

    /// The fast and fallback paths must produce (nearly) the same picture,
    /// for both chroma layouts the fast path handles.
    #[cfg(feature = "turbojpeg")]
    #[test]
    fn turbo_matches_rgb_path() {
        use turbojpeg::{Compressor, Image, PixelFormat, Subsamp};

        let (w, h) = (64usize, 48usize);
        let rgb: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8]
            })
            .collect();

        for subsamp in [Subsamp::Sub2x1, Subsamp::Sub2x2] {
            let mut compressor = Compressor::new().unwrap();
            compressor.set_quality(95);
            compressor.set_subsamp(subsamp);
            let image = Image {
                pixels: &rgb[..],
                width: w,
                pitch: w * 3,
                height: h,
                format: PixelFormat::RGB,
            };
            let jpeg = compressor.compress_to_vec(image).unwrap();

            let mut decoder = JpegDecoder::new().unwrap();
            let (mut fast, mut slow) = (Planes::default(), Planes::default());
            assert!(decoder.turbo.decode(&jpeg, &mut fast).unwrap());
            decoder.decode_rgb(&jpeg, &mut slow).unwrap();
            assert_eq!((fast.width, fast.height), (w, h));

            let (fast, slow) = (yuyv(&fast), yuyv(&slow));
            let diffs: Vec<i32> = fast
                .iter()
                .zip(&slow)
                .map(|(a, b)| (*a as i32 - *b as i32).abs())
                .collect();
            let mean = diffs.iter().sum::<i32>() as f64 / diffs.len() as f64;
            let max = *diffs.iter().max().unwrap();
            assert!(
                mean < 1.0 && max <= 4,
                "{subsamp:?}: mean {mean:.2}, max {max}"
            );
        }
    }

    #[test]
    fn rejects_garbage() {
        let mut decoder = JpegDecoder::new().unwrap();
        assert!(
            decoder
                .decode(b"not a jpeg", &mut Planes::default())
                .is_err()
        );
    }
}
