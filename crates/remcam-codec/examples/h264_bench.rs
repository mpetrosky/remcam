//! Measure the server's H.264 path on recorded camera frames:
//! JPEG decode, I420 conversion, encode, and (client side) decode.
//!
//! cargo run --release -p remcam-codec --example h264_bench -- DIR [KBPS] [FPS]
//!
//! DIR holds MJPEG frames as *.jpg, e.g. from
//! ffmpeg -f v4l2 -input_format mjpeg -video_size 1280x720 -i /dev/video0 -frames:v 90 -c copy DIR/%03d.jpg

use std::time::{Duration, Instant};

use remcam_codec::{Planes, h264, jpeg::JpegDecoder};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: h264_bench DIR [KBPS] [FPS]");
    let kbps: Option<u32> = args.next().map(|s| s.parse()).transpose()?;
    let fps: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(30);

    let mut paths: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
        .collect();
    paths.sort();
    let frames: Vec<Vec<u8>> = paths.iter().map(std::fs::read).collect::<Result<_, _>>()?;
    anyhow::ensure!(!frames.is_empty(), "no .jpg files in {dir}");

    let mut jpeg = JpegDecoder::new()?;
    let (mut planes, mut i420, mut decoded) =
        (Planes::default(), Planes::default(), Planes::default());
    jpeg.decode(&frames[0], &mut planes)?;
    let (w, h) = (planes.width as u32, planes.height as u32);
    let bitrate = kbps.map_or_else(|| h264::default_bitrate(w, h, fps), |k| k * 1000);
    let mut encoder = h264::Encoder::new(fps, bitrate)?;
    let mut decoder = h264::Decoder::new()?;
    let mut packet = Vec::new();

    let (mut t_jpeg, mut t_conv, mut t_enc, mut t_dec) = Default::default();
    let mut bytes = 0usize;
    let mut keyframe_bytes = 0usize;
    let mut keyframes = 0usize;
    let rounds = 3;
    for round in 0..rounds {
        for frame in &frames {
            let t = Instant::now();
            jpeg.decode(frame, &mut planes)?;
            let t1 = Instant::now();
            planes.to_i420(&mut i420);
            let t2 = Instant::now();
            let key = encoder.encode(&i420, &mut packet)?;
            let t3 = Instant::now();
            decoder.decode(&packet, &mut decoded)?;
            let t4 = Instant::now();
            add(&mut t_jpeg, t1 - t);
            add(&mut t_conv, t2 - t1);
            add(&mut t_enc, t3 - t2);
            add(&mut t_dec, t4 - t3);
            bytes += packet.len();
            keyframes += usize::from(key);
            if key && round == 0 {
                keyframe_bytes = packet.len();
            }
        }
    }
    let n = (frames.len() * rounds) as f64;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n;
    println!(
        "{w}x{h}: {} frames, target {:.2} Mbit/s at {fps} fps",
        frames.len(),
        bitrate as f64 / 1e6
    );
    println!(
        "per frame: jpeg {:.2} ms  i420 {:.2} ms  encode {:.2} ms  decode {:.2} ms",
        ms(t_jpeg),
        ms(t_conv),
        ms(t_enc),
        ms(t_dec)
    );
    println!(
        "output {:.2} Mbit/s at {fps} fps, {keyframes} keyframes, first {} KB",
        bytes as f64 * 8.0 * fps as f64 / n / 1e6,
        keyframe_bytes / 1024
    );
    Ok(())
}

fn add(total: &mut Duration, d: Duration) {
    *total += d;
}
