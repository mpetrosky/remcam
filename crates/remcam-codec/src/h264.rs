//! H.264 via OpenH264: low-latency encoding on the server, decoding on the
//! client. Streams are baseline-style (no B-frames), so every frame decodes
//! as soon as it arrives. Keyframes are only sent at the start and when the
//! client asks for one; QUIC delivers every frame, so the decoder only loses
//! sync if the client itself drops a frame.

use anyhow::{Context, ensure};
use openh264::OpenH264API;
use openh264::decoder::{Decoder as RawDecoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Complexity, Encoder as RawEncoder, EncoderConfig, FrameRate, FrameType,
    IntraFramePeriod, QpRange, RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use openh264_sys2::{
    ENCODER_OPTION, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo,
    SPATIAL_LAYER_ALL,
};

use crate::Planes;

/// A reasonable starting bitrate for a camera picture: 0.1 bits per pixel,
/// e.g. 2.8 Mbit/s for 720p30 and 6.2 Mbit/s for 1080p30.
pub fn default_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    let bps = u64::from(width) * u64::from(height) * u64::from(fps.max(1)) / 10;
    bps.clamp(300_000, 20_000_000) as u32
}

pub struct Encoder {
    inner: RawEncoder,
    /// Bitrate the encoder is currently configured for, in bits per second.
    applied: u32,
    target: u32,
    started: bool,
}

impl Encoder {
    pub fn new(fps: u32, bitrate: u32) -> anyhow::Result<Self> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
            // Frames are dropped before encoding when the link is busy; the
            // encoder itself must encode everything it is given.
            .skip_frames(false)
            .complexity(Complexity::Low)
            // A minimum QP of 0 makes OpenH264 replace the whole range with
            // its own default, which tops out at 42. Noisy pictures then
            // can't get down to low bitrates, so set the range explicitly.
            .qp(QpRange::new(12, 51))
            // Keyframes only on request: a scene-change keyframe is a burst
            // several times the size of a normal frame, which shows up as a
            // latency spike, and noisy pictures (a dark room) trigger it often.
            .scene_change_detect(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .vui(VuiConfig::bt601())
            // OpenH264 only spreads work over threads (one per core by
            // default) when a frame has several slices. Size-limited slices
            // about halve encode time, e.g. 23 -> 10 ms at 1080p.
            .max_slice_len(8000);
        let inner = RawEncoder::with_api_config(OpenH264API::from_source(), config)
            .context("creating H.264 encoder")?;
        Ok(Self {
            inner,
            applied: bitrate,
            target: bitrate,
            started: false,
        })
    }

    /// Change the bitrate (bits per second) from the next frame on.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.target = bitrate;
    }

    pub fn bitrate(&self) -> u32 {
        self.target
    }

    /// Make the next frame a keyframe (IDR), which decodes on its own.
    pub fn force_keyframe(&mut self) {
        // Before the first frame the encoder isn't initialized, and the
        // first frame is a keyframe anyway.
        if self.started {
            self.inner.force_intra_frame();
        }
    }

    /// Encode a limited-range I420 picture into `out` (Annex B byte stream).
    /// Returns whether the result is a keyframe.
    pub fn encode(&mut self, picture: &Planes, out: &mut Vec<u8>) -> anyhow::Result<bool> {
        ensure!(
            picture.is_420() && !picture.full_range,
            "encoder needs limited-range I420"
        );
        ensure!(
            picture.height.is_multiple_of(2),
            "H.264 needs an even height, got {}",
            picture.height
        );
        if self.started && self.applied != self.target {
            self.apply_bitrate()?;
        }
        let stream = self.inner.encode(picture).context("encoding frame")?;
        self.started = true;
        out.clear();
        stream.write_vec(out);
        Ok(matches!(stream.frame_type(), FrameType::IDR | FrameType::I))
    }

    fn apply_bitrate(&mut self) -> anyhow::Result<()> {
        // The maximum must never be below the target, so the order depends
        // on the direction of the change.
        let options = if self.target > self.applied {
            [ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_BITRATE]
        } else {
            [ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE]
        };
        for option in options {
            self.set_option(option, self.target)?;
        }
        self.applied = self.target;
        Ok(())
    }

    fn set_option(&mut self, option: ENCODER_OPTION, bitrate: u32) -> anyhow::Result<()> {
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: bitrate.try_into()?,
        };
        // SAFETY: the encoder is initialized (a frame was encoded), and both
        // bitrate options take a pointer to an SBitrateInfo that lives
        // across the call. They only change rate control targets.
        let rc = unsafe {
            self.inner
                .raw_api()
                .set_option(option, (&raw mut info).cast())
        };
        ensure!(rc == 0, "setting H.264 bitrate to {bitrate} failed ({rc})");
        Ok(())
    }
}

impl YUVSource for Planes {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.chroma_width(), self.chroma_width())
    }

    fn y(&self) -> &[u8] {
        Planes::y(self)
    }

    fn u(&self) -> &[u8] {
        Planes::u(self)
    }

    fn v(&self) -> &[u8] {
        Planes::v(self)
    }
}

pub struct Decoder {
    inner: RawDecoder,
}

impl Decoder {
    pub fn new() -> anyhow::Result<Self> {
        let inner = RawDecoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .context("creating H.264 decoder")?;
        Ok(Self { inner })
    }

    /// Decode one frame into `out` (limited-range I420). Returns `false` if
    /// the data produced no picture (e.g. parameter sets only). An error
    /// usually means a reference frame is missing; decoding resumes cleanly
    /// at the next keyframe.
    pub fn decode(&mut self, frame: &[u8], out: &mut Planes) -> anyhow::Result<bool> {
        let Some(yuv) = self.inner.decode(frame).context("decoding H.264")? else {
            return Ok(false);
        };
        let (w, h) = yuv.dimensions();
        let (y_stride, u_stride, v_stride) = yuv.strides();
        out.reset(w, h, h.div_ceil(2), false);
        let (cw, ch) = (out.chroma_width(), out.chroma_height);
        let (oy, ou, ov) = out.planes_mut();
        for (src, stride, dst, width, rows) in [
            (yuv.y(), y_stride, oy, w, h),
            (yuv.u(), u_stride, ou, cw, ch),
            (yuv.v(), v_stride, ov, cw, ch),
        ] {
            for (row, dst_row) in dst.chunks_exact_mut(width).take(rows).enumerate() {
                dst_row.copy_from_slice(&src[row * stride..][..width]);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moving gradient, limited-range I420.
    fn picture(w: usize, h: usize, t: usize) -> Planes {
        let mut p = Planes::default();
        p.reset(w, h, h / 2, false);
        let (y, u, v) = p.planes_mut();
        for (i, b) in y.iter_mut().enumerate() {
            *b = 16 + ((i % w + i / w + t * 3) % 200) as u8;
        }
        u.fill(100);
        v.fill(150);
        p
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse = a
            .iter()
            .zip(b)
            .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
            .sum::<f64>()
            / a.len() as f64;
        10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
    }

    #[test]
    fn roundtrip_and_keyframes() {
        let (w, h) = (160, 96);
        let mut enc = Encoder::new(30, 1_000_000).unwrap();
        let mut dec = Decoder::new().unwrap();
        let (mut packet, mut out) = (Vec::new(), Planes::default());

        let mut kinds = Vec::new();
        for t in 0..6 {
            if t == 4 {
                enc.force_keyframe();
            }
            if t == 2 {
                enc.set_bitrate(500_000);
            }
            let input = picture(w, h, t);
            kinds.push(enc.encode(&input, &mut packet).unwrap());
            assert!(dec.decode(&packet, &mut out).unwrap());
            assert_eq!((out.width, out.height), (w, h));
            let q = psnr(out.y(), input.y());
            assert!(q > 30.0, "frame {t}: PSNR {q:.1} dB");
        }
        assert_eq!(kinds, [true, false, false, false, true, false]);
    }

    #[test]
    fn decoder_recovers_at_keyframe() {
        let (w, h) = (160, 96);
        let mut enc = Encoder::new(30, 1_000_000).unwrap();
        let mut packets = Vec::new();
        for t in 0..4 {
            if t == 3 {
                enc.force_keyframe();
            }
            let mut p = Vec::new();
            enc.encode(&picture(w, h, t), &mut p).unwrap();
            packets.push(p);
        }
        // A fresh decoder (as after a client-side drop) that starts with a
        // P-frame can't show it, but recovers at the keyframe.
        let mut dec = Decoder::new().unwrap();
        let mut out = Planes::default();
        assert!(!matches!(dec.decode(&packets[1], &mut out), Ok(true)));
        assert!(dec.decode(&packets[3], &mut out).unwrap());
    }

    #[test]
    fn default_bitrates() {
        assert_eq!(default_bitrate(1280, 720, 30), 2_764_800);
        assert_eq!(default_bitrate(160, 120, 5), 300_000);
    }
}
