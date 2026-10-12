//! Planar YUV pictures and conversions between layouts and ranges.

/// JPEG (JFIF) stores full-range YCbCr; video formats (YUYV, H.264) use
/// BT.601 limited range (Y 16–235, Cb/Cr 16–240).
const Y_LUT: [u8; 256] = range_lut(219);
const C_LUT: [u8; 256] = range_lut(224);
const IDENTITY: [u8; 256] = identity_lut();

const fn range_lut(span: u32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        lut[i] = (16 + (i as u32 * span + 127) / 255) as u8;
        i += 1;
    }
    lut
}

const fn identity_lut() -> [u8; 256] {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        lut[i] = i as u8;
        i += 1;
    }
    lut
}

/// A planar YUV picture: the Y plane, then U and V at half width and either
/// full height (4:2:2) or half height (4:2:0). Width must be even.
#[derive(Debug, Default, Clone)]
pub struct Planes {
    pub width: usize,
    pub height: usize,
    /// `height` for 4:2:2, `height.div_ceil(2)` for 4:2:0.
    pub chroma_height: usize,
    /// Full range (0–255, as decoded from JPEG) rather than limited range.
    pub full_range: bool,
    data: Vec<u8>,
}

impl Planes {
    /// Resize for a new picture; the contents are left unspecified.
    pub fn reset(&mut self, width: usize, height: usize, chroma_height: usize, full_range: bool) {
        debug_assert!(width.is_multiple_of(2));
        *self = Planes {
            width,
            height,
            chroma_height,
            full_range,
            data: std::mem::take(&mut self.data),
        };
        let len = width * height + 2 * self.chroma_width() * chroma_height;
        self.data.resize(len, 0);
    }

    pub fn chroma_width(&self) -> usize {
        self.width / 2
    }

    pub fn is_420(&self) -> bool {
        self.chroma_height < self.height
    }

    pub fn y(&self) -> &[u8] {
        &self.data[..self.width * self.height]
    }

    pub fn u(&self) -> &[u8] {
        let (y, c) = (
            self.width * self.height,
            self.chroma_width() * self.chroma_height,
        );
        &self.data[y..y + c]
    }

    pub fn v(&self) -> &[u8] {
        let (y, c) = (
            self.width * self.height,
            self.chroma_width() * self.chroma_height,
        );
        &self.data[y + c..y + 2 * c]
    }

    /// All three planes, contiguous, as libjpeg-turbo's YUV functions expect.
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let (y, c) = (
            self.width * self.height,
            self.chroma_width() * self.chroma_height,
        );
        let (y_plane, chroma) = self.data.split_at_mut(y);
        let (u, v) = chroma.split_at_mut(c);
        (y_plane, u, v)
    }

    fn luts(&self) -> (&'static [u8; 256], &'static [u8; 256]) {
        if self.full_range {
            (&Y_LUT, &C_LUT)
        } else {
            (&IDENTITY, &IDENTITY)
        }
    }

    /// Write packed limited-range YUYV into `out` (`width * height * 2` bytes).
    pub fn to_yuyv(&self, out: &mut [u8]) {
        let (w, cw) = (self.width, self.chroma_width());
        debug_assert_eq!(out.len(), w * self.height * 2);
        let (y_lut, c_lut) = self.luts();
        let rows_per_chroma_row = if self.is_420() { 2 } else { 1 };
        let (y, u, v) = (self.y(), self.u(), self.v());
        for (row, out_row) in out.chunks_exact_mut(w * 2).enumerate() {
            let c = (row / rows_per_chroma_row) * cw;
            let pairs = y[row * w..][..w]
                .as_chunks::<2>()
                .0
                .iter()
                .zip(&u[c..c + cw])
                .zip(&v[c..c + cw]);
            for (o, ((yy, &cb), &cr)) in out_row.as_chunks_mut::<4>().0.iter_mut().zip(pairs) {
                *o = [
                    y_lut[yy[0] as usize],
                    c_lut[cb as usize],
                    y_lut[yy[1] as usize],
                    c_lut[cr as usize],
                ];
            }
        }
    }

    /// Convert to limited-range 4:2:0 (I420) in `out`, averaging vertical
    /// chroma pairs when the source is 4:2:2.
    pub fn to_i420(&self, out: &mut Planes) {
        let (w, h, cw) = (self.width, self.height, self.chroma_width());
        out.reset(w, h, h.div_ceil(2), false);
        let (y_lut, c_lut) = self.luts();
        let (oy, ou, ov) = out.planes_mut();
        for (o, &i) in oy.iter_mut().zip(self.y()) {
            *o = y_lut[i as usize];
        }
        for (src, dst) in [(self.u(), ou), (self.v(), ov)] {
            if self.is_420() {
                for (o, &i) in dst.iter_mut().zip(src) {
                    *o = c_lut[i as usize];
                }
                continue;
            }
            for (r, dst_row) in dst.chunks_exact_mut(cw).enumerate() {
                let a = &src[2 * r * cw..][..cw];
                let b = &src[(2 * r + 1).min(h - 1) * cw..][..cw];
                for ((o, &a), &b) in dst_row.iter_mut().zip(a).zip(b) {
                    *o = c_lut[(a as usize + b as usize).div_ceil(2)];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(chroma_height: usize, full_range: bool) -> Planes {
        let mut p = Planes::default();
        p.reset(4, 4, chroma_height, full_range);
        let (y, u, v) = p.planes_mut();
        y.iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b = i as u8 * 10);
        u.iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b = 100 + i as u8);
        v.iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b = 200 + i as u8);
        p
    }

    #[test]
    fn range_conversion_endpoints() {
        assert_eq!((Y_LUT[0], Y_LUT[255]), (16, 235));
        assert_eq!((C_LUT[0], C_LUT[128], C_LUT[255]), (16, 128, 240));
    }

    #[test]
    fn yuyv_from_420_repeats_chroma_rows() {
        let p = picture(2, false);
        let mut out = [0u8; 4 * 4 * 2];
        p.to_yuyv(&mut out);
        // Row 0 and row 1 share chroma row 0; row 2 uses chroma row 1.
        assert_eq!(&out[..8], &[0, 100, 10, 200, 20, 101, 30, 201]);
        assert_eq!(&out[8..16], &[40, 100, 50, 200, 60, 101, 70, 201]);
        assert_eq!(&out[16..20], &[80, 102, 90, 202]);
    }

    #[test]
    fn i420_from_422_averages_rows() {
        let p = picture(4, false);
        let mut out = Planes::default();
        p.to_i420(&mut out);
        assert_eq!(out.chroma_height, 2);
        assert_eq!(out.y(), p.y());
        // u rows: [100,101] [102,103] [104,105] [106,107]
        assert_eq!(out.u(), &[101, 102, 105, 106]);
    }

    #[test]
    fn i420_from_full_range_is_limited() {
        let mut p = Planes::default();
        p.reset(2, 2, 1, true);
        p.data_mut().copy_from_slice(&[0, 255, 0, 255, 0, 255]);
        let mut out = Planes::default();
        p.to_i420(&mut out);
        assert!(!out.full_range);
        assert_eq!(out.y(), &[16, 235, 16, 235]);
        assert_eq!((out.u(), out.v()), (&[16][..], &[240][..]));
    }
}
