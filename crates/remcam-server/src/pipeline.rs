//! One open camera stream: capture → encode → one QUIC stream per frame.
//!
//! ```text
//! capture thread ──Frame──▶ encode thread ──Packet──▶ sender task ──▶ QUIC
//!                                ▲                          │
//!                    bitrate, keyframe requests     (frames in flight)
//!                                ▲
//!                         rate controller (H.264)
//! ```
//!
//! At most [`MAX_INFLIGHT_FRAMES`] frames are in flight at once. When that
//! many are still being sent, new frames are dropped *before* encoding: the
//! encoder never sees them, so later H.264 frames don't reference a frame
//! the client never got.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, bail};
use remcam_codec::{Planes, h264, jpeg::JpegDecoder};
use remcam_proto::{Codec, FrameHeader, StreamOptions, VideoFormat, write_msg};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::{debug, info, warn};

use crate::camera::{Capture, Frame};
use crate::rate::{self, Change, RateController, Sample};

/// Frames sent but not yet acknowledged by the client. Beyond this, new
/// frames are dropped instead of queued, which bounds latency when the link
/// is slow, and tells the rate controller to back off.
const MAX_INFLIGHT_FRAMES: usize = 4;

pub struct Pipeline {
    capture: Capture,
    encoder: tokio::task::JoinHandle<()>,
    rate: Option<tokio::task::JoinHandle<()>>,
    keyframe: Arc<AtomicBool>,
}

/// An encoded frame, ready to send. Holds its in-flight slot until the
/// client has acknowledged it.
struct Packet {
    seq: u64,
    capture_ts_us: u64,
    keyframe: bool,
    data: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}

impl Pipeline {
    /// Open `device` and start sending frames on `conn`. Returns the camera
    /// format and stream options actually in effect.
    pub async fn start(
        conn: quinn::Connection,
        epoch: u32,
        device: String,
        format: VideoFormat,
        options: StreamOptions,
    ) -> anyhow::Result<(Self, VideoFormat, StreamOptions)> {
        let (frame_tx, frame_rx) = mpsc::channel(1);
        let (capture, actual) = Capture::start(device, format, frame_tx).await?;

        let keyframe = Arc::new(AtomicBool::new(false));
        let bitrate = Arc::new(AtomicU32::new(0));
        let (encoder, options) = match FrameEncoder::new(actual, options, &bitrate, &keyframe) {
            Ok(e) => e,
            Err(e) => {
                capture.stop().await;
                return Err(e);
            }
        };

        let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_FRAMES));
        let dropped = Arc::new(AtomicU64::new(0));
        let (packet_tx, packet_rx) = mpsc::channel(MAX_INFLIGHT_FRAMES);
        let encoder = tokio::task::spawn_blocking({
            let dropped = dropped.clone();
            move || encode_loop(frame_rx, encoder, inflight, dropped, packet_tx)
        });
        tokio::spawn(send_packets(conn.clone(), epoch, packet_rx));
        let rate = options.max_bitrate.map(|max| {
            tokio::spawn(adapt_bitrate(
                conn,
                dropped,
                bitrate,
                RateController::new(max),
            ))
        });
        Ok((
            Self {
                capture,
                encoder,
                rate,
                keyframe,
            },
            actual,
            options,
        ))
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// Stop capturing and wait until the device is released.
    pub async fn stop(self) {
        self.capture.stop().await;
        // The encoder ends once the capture side has hung up.
        let _ = self.encoder.await;
        if let Some(rate) = self.rate {
            rate.abort();
        }
    }
}

enum FrameEncoder {
    /// MJPEG: the camera's frames as they are.
    Passthrough,
    H264(Box<H264Encoder>),
}

/// Camera JPEG → I420 → H.264.
struct H264Encoder {
    jpeg: JpegDecoder,
    planes: Planes,
    i420: Planes,
    encoder: h264::Encoder,
    bitrate: Arc<AtomicU32>,
    keyframe: Arc<AtomicBool>,
    errors: u64,
}

impl FrameEncoder {
    fn new(
        format: VideoFormat,
        options: StreamOptions,
        bitrate: &Arc<AtomicU32>,
        keyframe: &Arc<AtomicBool>,
    ) -> anyhow::Result<(Self, StreamOptions)> {
        match options.codec {
            Codec::Mjpeg => Ok((
                FrameEncoder::Passthrough,
                StreamOptions {
                    codec: Codec::Mjpeg,
                    max_bitrate: None,
                },
            )),
            Codec::H264 => {
                if !format.width.is_multiple_of(2) || !format.height.is_multiple_of(2) {
                    bail!("H.264 needs even dimensions, the camera gives {format}");
                }
                let max = options.max_bitrate.unwrap_or_else(|| {
                    h264::default_bitrate(format.width, format.height, format.fps)
                });
                bitrate.store(max, Ordering::Relaxed);
                let encoder = H264Encoder {
                    jpeg: JpegDecoder::new()?,
                    planes: Planes::default(),
                    i420: Planes::default(),
                    encoder: h264::Encoder::new(format.fps, max)?,
                    bitrate: bitrate.clone(),
                    keyframe: keyframe.clone(),
                    errors: 0,
                };
                let options = StreamOptions {
                    codec: Codec::H264,
                    max_bitrate: Some(max),
                };
                Ok((FrameEncoder::H264(Box::new(encoder)), options))
            }
        }
    }

    /// Returns the payload to send and whether it is a keyframe, or `None`
    /// to skip the frame.
    fn encode(&mut self, jpeg_frame: Vec<u8>) -> Option<(Vec<u8>, bool)> {
        match self {
            FrameEncoder::Passthrough => Some((jpeg_frame, true)),
            FrameEncoder::H264(h264) => h264.encode(&jpeg_frame),
        }
    }
}

impl H264Encoder {
    fn encode(&mut self, jpeg_frame: &[u8]) -> Option<(Vec<u8>, bool)> {
        self.encoder
            .set_bitrate(self.bitrate.load(Ordering::Relaxed));
        if self.keyframe.swap(false, Ordering::Relaxed) {
            debug!("keyframe requested");
            self.encoder.force_keyframe();
        }
        let result = self
            .jpeg
            .decode(jpeg_frame, &mut self.planes)
            .and_then(|()| {
                self.planes.to_i420(&mut self.i420);
                let mut out = Vec::with_capacity(jpeg_frame.len() / 4);
                let key = self.encoder.encode(&self.i420, &mut out)?;
                Ok((out, key))
            });
        match result {
            Ok((out, _)) if out.is_empty() => None,
            Ok(packet) => Some(packet),
            Err(e) => {
                // Cameras occasionally deliver a corrupt JPEG. Skipping it
                // is harmless: the encoder never saw it.
                self.errors += 1;
                if self.errors.is_power_of_two() {
                    warn!("skipping frame ({} so far): {e:#}", self.errors);
                }
                None
            }
        }
    }
}

fn encode_loop(
    mut rx: mpsc::Receiver<Frame>,
    mut encoder: FrameEncoder,
    inflight: Arc<Semaphore>,
    dropped: Arc<AtomicU64>,
    tx: mpsc::Sender<Packet>,
) {
    while let Some(frame) = rx.blocking_recv() {
        let Ok(permit) = inflight.clone().try_acquire_owned() else {
            let n = dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                debug!("link busy, {n} frames dropped so far");
            }
            continue;
        };
        let Some((data, keyframe)) = encoder.encode(frame.data) else {
            continue;
        };
        let packet = Packet {
            seq: frame.seq,
            capture_ts_us: frame.capture_ts_us,
            keyframe,
            data,
            _permit: permit,
        };
        if tx.blocking_send(packet).is_err() {
            break;
        }
    }
}

/// Send each packet on its own unidirectional stream. Streams are opened
/// here, one after the other, so their order matches the frame order.
async fn send_packets(conn: quinn::Connection, epoch: u32, mut rx: mpsc::Receiver<Packet>) {
    while let Some(packet) = rx.recv().await {
        let stream = match conn.open_uni().await {
            Ok(s) => s,
            Err(e) => return debug!("opening frame stream: {e}"),
        };
        tokio::spawn(async move {
            if let Err(e) = send_packet(stream, epoch, packet).await {
                debug!("frame send failed: {e:#}");
            }
        });
    }
}

async fn send_packet(
    mut stream: quinn::SendStream,
    epoch: u32,
    packet: Packet,
) -> anyhow::Result<()> {
    let header = FrameHeader {
        epoch,
        seq: packet.seq,
        capture_ts_us: packet.capture_ts_us,
        len: packet.data.len().try_into().context("frame too large")?,
        keyframe: packet.keyframe,
    };
    write_msg(&mut stream, &header).await?;
    stream.write_all(&packet.data).await?;
    stream.finish()?;
    // `write_all` returns once QUIC has buffered the data, which can be
    // seconds ahead of a slow link. Keep the in-flight slot until the client
    // has acknowledged the whole frame (or stopped it as stale).
    let _ = stream.stopped().await;
    Ok(())
}

async fn adapt_bitrate(
    conn: quinn::Connection,
    dropped: Arc<AtomicU64>,
    bitrate: Arc<AtomicU32>,
    mut controller: RateController,
) {
    let remote = conn.remote_address();
    let mut tick = tokio::time::interval(rate::INTERVAL);
    loop {
        tick.tick().await;
        let sample = Sample::from_connection(&conn, dropped.load(Ordering::Relaxed));
        let mbps = |c: &RateController| c.target() as f64 / 1e6;
        match controller.update(Instant::now(), sample) {
            Some(Change::Down(why)) => {
                info!(
                    "{remote}: bitrate down to {:.2} Mbit/s ({why})",
                    mbps(&controller)
                );
            }
            Some(Change::Up) => debug!("{remote}: bitrate up to {:.2} Mbit/s", mbps(&controller)),
            None => {}
        }
        bitrate.store(controller.target(), Ordering::Relaxed);
    }
}
