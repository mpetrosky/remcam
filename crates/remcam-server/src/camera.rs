//! V4L2 device discovery and capture.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use remcam_proto::{DeviceInfo, FormatInfo, FourCC, VideoFormat};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use v4l::Device;
use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::framesize::FrameSizeEnum;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture as _;
use v4l::video::capture::Parameters;

/// Number of driver buffers. More buffers tolerate scheduling hiccups but add latency.
const BUFFER_COUNT: u32 = 4;
/// Give up if the camera produces no frame for this long.
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Frame {
    pub seq: u64,
    pub capture_ts_us: u64,
    pub data: Vec<u8>,
}

/// Enumerate capture devices that can produce MJPEG.
///
/// UVC cameras expose a second "metadata" node per camera, which has no
/// video capture capability; those are skipped.
pub fn list_devices() -> Vec<DeviceInfo> {
    let mut paths: Vec<_> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("video"))
        })
        .collect();
    paths.sort_by_key(|p| {
        p.to_string_lossy()
            .trim_start_matches("/dev/video")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });

    paths
        .iter()
        .filter_map(|p| match describe(p) {
            Ok(info) => info,
            Err(e) => {
                debug!("skipping {}: {e:#}", p.display());
                None
            }
        })
        .collect()
}

fn describe(path: &Path) -> anyhow::Result<Option<DeviceInfo>> {
    let dev = Device::with_path(path)?;
    let caps = dev.query_caps()?;
    if !caps
        .capabilities
        .contains(Flags::VIDEO_CAPTURE | Flags::STREAMING)
    {
        return Ok(None);
    }
    let mut formats = Vec::new();
    for desc in dev.enum_formats()? {
        let fourcc = FourCC(desc.fourcc.repr);
        // M1 only streams the camera's own MJPEG; raw formats come with encoding (M4).
        if fourcc != FourCC::MJPG {
            continue;
        }
        let sizes = dev
            .enum_framesizes(desc.fourcc)?
            .into_iter()
            .filter_map(|fs| match fs.size {
                FrameSizeEnum::Discrete(d) => Some((d.width, d.height)),
                FrameSizeEnum::Stepwise(_) => None,
            })
            .collect();
        formats.push(FormatInfo { fourcc, sizes });
    }
    if formats.is_empty() {
        return Ok(None);
    }
    Ok(Some(DeviceInfo {
        id: path.to_string_lossy().into_owned(),
        name: caps.card,
        formats,
    }))
}

/// A running capture thread.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: tokio::task::JoinHandle<()>,
}

impl Capture {
    /// Open `path`, apply `requested`, and start capturing into `tx`.
    ///
    /// Returns once the device is configured, with the format the driver
    /// actually accepted (which may differ from the request). Frames are
    /// dropped, not queued, when `tx` is full, so a slow network never adds
    /// latency on the capture side.
    pub async fn start(
        path: String,
        requested: VideoFormat,
        tx: mpsc::Sender<Frame>,
    ) -> anyhow::Result<(Self, VideoFormat)> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread = tokio::task::spawn_blocking({
            let stop = stop.clone();
            move || {
                let stream = match configure(&path, requested) {
                    Ok((stream, actual)) => {
                        let _ = ready_tx.send(Ok(actual));
                        stream
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                match capture_loop(stream, &tx, &stop) {
                    Ok(()) => debug!("capture on {path} stopped"),
                    Err(e) => warn!("capture on {path} failed: {e:#}"),
                }
            }
        });
        let actual = ready_rx.await.context("capture thread exited")??;
        Ok((Self { stop, thread }, actual))
    }

    /// Stop capturing and wait until the device is released.
    pub async fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.await;
    }
}

fn configure(path: &str, req: VideoFormat) -> anyhow::Result<(Stream<'static>, VideoFormat)> {
    let dev = Device::with_path(path).with_context(|| format!("opening {path}"))?;

    let mut fmt = dev.format()?;
    fmt.width = req.width;
    fmt.height = req.height;
    fmt.fourcc = v4l::FourCC::new(&req.fourcc.0);
    let fmt = dev.set_format(&fmt).context("setting format")?;
    if fmt.fourcc.repr != req.fourcc.0 {
        bail!("device does not support {}", req.fourcc);
    }

    let params = dev
        .set_params(&Parameters::with_fps(req.fps))
        .context("setting frame rate")?;
    let fps = match params.interval {
        f if f.numerator > 0 => f.denominator / f.numerator,
        _ => req.fps,
    };
    let actual = VideoFormat {
        fourcc: req.fourcc,
        width: fmt.width,
        height: fmt.height,
        fps,
    };

    let mut stream = Stream::with_buffers(&dev, Type::VideoCapture, BUFFER_COUNT)
        .context("allocating capture buffers")?;
    stream.set_timeout(FRAME_TIMEOUT);
    info!("{path}: capturing {actual}");
    Ok((stream, actual))
}

fn capture_loop(
    mut stream: Stream<'_>,
    tx: &mpsc::Sender<Frame>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    // V4L2 buffer timestamps are CLOCK_MONOTONIC; convert them to wall-clock
    // time so the client can estimate end-to-end latency.
    let mono_to_wall_us = wall_now_us() as i64 - monotonic_now_us();
    let mut seq = 0u64;
    let mut dropped = 0u64;

    while !stop.load(Ordering::Relaxed) {
        let (buf, meta) = stream.next().context("dequeuing frame")?;
        let used = (meta.bytesused as usize).min(buf.len());
        if used == 0 {
            continue;
        }
        let mono_us = meta.timestamp.sec * 1_000_000 + meta.timestamp.usec;
        let frame = Frame {
            seq,
            capture_ts_us: (mono_us + mono_to_wall_us) as u64,
            data: buf[..used].to_vec(),
        };
        seq += 1;
        match tx.try_send(frame) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped += 1;
                if dropped.is_power_of_two() {
                    debug!("sender busy, {dropped} frames dropped so far");
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }
    Ok(())
}

fn wall_now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn monotonic_now_us() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec and CLOCK_MONOTONIC is always available.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000
}
