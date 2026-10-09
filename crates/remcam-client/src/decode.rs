//! MJPEG → YUYV conversion.
//!
//! With the `turbojpeg` feature, libjpeg-turbo decodes straight to planar
//! YUV, which only needs interleaving and a range conversion. That's about
//! 2x faster than the pure-Rust `zune-jpeg` path, which can only produce RGB
//! and therefore needs a full color conversion. The `zune-jpeg` path is also
//! used for chroma layouts the fast path doesn't handle.

use anyhow::{Context, bail};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;

pub struct JpegToYuyv {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
    #[cfg(feature = "turbojpeg")]
    turbo: turbo::Turbo,
}

impl JpegToYuyv {
    pub fn new(width: usize, height: usize) -> anyhow::Result<Self> {
        if !width.is_multiple_of(2) {
            bail!("YUYV needs an even width, got {width}");
        }
        Ok(Self {
            width,
            height,
            rgb: Vec::new(),
            #[cfg(feature = "turbojpeg")]
            turbo: turbo::Turbo::new()?,
        })
    }

    /// Decode `jpeg` into `out`, which must hold `width * height * 2` bytes.
    pub fn decode(&mut self, jpeg: &[u8], out: &mut [u8]) -> anyhow::Result<()> {
        debug_assert_eq!(out.len(), self.width * self.height * 2);
        #[cfg(feature = "turbojpeg")]
        if self.turbo.decode(jpeg, self.width, self.height, out)? {
            return Ok(());
        }
        self.decode_rgb(jpeg, out)
    }

    fn decode_rgb(&mut self, jpeg: &[u8], out: &mut [u8]) -> anyhow::Result<()> {
        let mut decoder = JpegDecoder::new(ZCursor::new(jpeg));
        decoder.decode_headers().context("bad JPEG header")?;
        let info = decoder.info().context("bad JPEG header")?;
        check_size(
            info.width.into(),
            info.height.into(),
            self.width,
            self.height,
        )?;
        self.rgb.resize(self.width * self.height * 3, 0);
        decoder
            .decode_into(&mut self.rgb)
            .context("decoding JPEG")?;
        rgb_to_yuyv(&self.rgb, out);
        Ok(())
    }
}

fn check_size(w: usize, h: usize, want_w: usize, want_h: usize) -> anyhow::Result<()> {
    if (w, h) != (want_w, want_h) {
        bail!("frame is {w}x{h}, expected {want_w}x{want_h}");
    }
    Ok(())
}

/// Convert packed RGB to YUYV (4:2:2) using BT.601 limited-range coefficients.
/// Chroma is taken from the average of each horizontal pixel pair.
fn rgb_to_yuyv(rgb: &[u8], yuyv: &mut [u8]) {
    #[inline(always)]
    fn luma(r: i32, g: i32, b: i32) -> u8 {
        (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
    }
    for (px, out) in rgb
        .as_chunks::<6>()
        .0
        .iter()
        .zip(yuyv.as_chunks_mut::<4>().0)
    {
        let [r0, g0, b0, r1, g1, b1] = px.map(i32::from);
        let (r, g, b) = ((r0 + r1) / 2, (g0 + g1) / 2, (b0 + b1) / 2);
        out[0] = luma(r0, g0, b0);
        out[1] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
        out[2] = luma(r1, g1, b1);
        out[3] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
    }
}

#[cfg(feature = "turbojpeg")]
mod turbo {
    use anyhow::Context;
    use tracing::debug;
    use turbojpeg::{Decompressor, Subsamp, YuvImage};

    use super::check_size;

    /// JPEG (JFIF) stores full-range YCbCr; YUYV consumers expect BT.601
    /// limited range (Y 16–235, Cb/Cr 16–240).
    const Y_LUT: [u8; 256] = range_lut(219);
    const C_LUT: [u8; 256] = range_lut(224);

    const fn range_lut(span: u32) -> [u8; 256] {
        let mut lut = [0u8; 256];
        let mut i = 0;
        while i < 256 {
            lut[i] = (16 + (i as u32 * span + 127) / 255) as u8;
            i += 1;
        }
        lut
    }

    pub struct Turbo {
        decompressor: Decompressor,
        planes: Vec<u8>,
        warned: bool,
    }

    impl Turbo {
        pub fn new() -> anyhow::Result<Self> {
            Ok(Self {
                decompressor: Decompressor::new().context("initializing libjpeg-turbo")?,
                planes: Vec::new(),
                warned: false,
            })
        }

        /// Returns `Ok(false)` if the frame's chroma layout isn't handled
        /// here, so the caller can fall back to the RGB path.
        pub fn decode(
            &mut self,
            jpeg: &[u8],
            width: usize,
            height: usize,
            out: &mut [u8],
        ) -> anyhow::Result<bool> {
            let header = self
                .decompressor
                .read_header(jpeg)
                .context("bad JPEG header")?;
            check_size(header.width, header.height, width, height)?;
            // UVC cameras send 4:2:2; 4:2:0 shows up with some devices.
            let chroma_height = match header.subsamp {
                Subsamp::Sub2x1 => height,
                Subsamp::Sub2x2 => height.div_ceil(2),
                other => {
                    if !self.warned {
                        debug!("{other:?} JPEG, using the slower RGB path");
                        self.warned = true;
                    }
                    return Ok(false);
                }
            };

            let chroma_width = width / 2;
            self.planes
                .resize(width * height + 2 * chroma_width * chroma_height, 0);
            let image = YuvImage {
                pixels: &mut self.planes[..],
                width,
                align: 1,
                height,
                subsamp: header.subsamp,
            };
            self.decompressor
                .decompress_to_yuv(jpeg, image)
                .context("decoding JPEG")?;

            let (y, chroma) = self.planes.split_at(width * height);
            let (u, v) = chroma.split_at(chroma_width * chroma_height);
            let rows_per_chroma_row = height / chroma_height.max(1);
            for (row, out_row) in out.chunks_exact_mut(width * 2).enumerate() {
                let c = (row / rows_per_chroma_row) * chroma_width;
                let y_row = &y[row * width..][..width];
                let pairs = y_row
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(&u[c..c + chroma_width])
                    .zip(&v[c..c + chroma_width]);
                for (o, ((yy, &cb), &cr)) in out_row.as_chunks_mut::<4>().0.iter_mut().zip(pairs) {
                    *o = [
                        Y_LUT[yy[0] as usize],
                        C_LUT[cb as usize],
                        Y_LUT[yy[1] as usize],
                        C_LUT[cr as usize],
                    ];
                }
            }
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_reference_colors() {
        // white, black -> Y 235/16, neutral chroma
        let mut out = [0u8; 4];
        rgb_to_yuyv(&[255, 255, 255, 0, 0, 0], &mut out);
        assert_eq!(out[0], 235);
        assert_eq!(out[2], 16);
        assert!((127..=129).contains(&out[1]) && (127..=129).contains(&out[3]));

        // pure red: U below neutral, V near max (BT.601: Y=82, U=90, V=240)
        rgb_to_yuyv(&[255, 0, 0, 255, 0, 0], &mut out);
        assert_eq!(out, [82, 90, 82, 240]);
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

            let mut decoder = JpegToYuyv::new(w, h).unwrap();
            let (mut fast, mut slow) = (vec![0u8; w * h * 2], vec![0u8; w * h * 2]);
            assert!(decoder.turbo.decode(&jpeg, w, h, &mut fast).unwrap());
            decoder.decode_rgb(&jpeg, &mut slow).unwrap();

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
    fn rejects_wrong_size() {
        let mut decoder = JpegToYuyv::new(4, 4).unwrap();
        let mut out = vec![0u8; 4 * 4 * 2];
        assert!(decoder.decode(b"not a jpeg", &mut out).is_err());
    }
}
