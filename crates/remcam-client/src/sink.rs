//! Output to a v4l2loopback device, so that any V4L2 application sees the
//! remote camera as a local one.
//!
//! Frames arrive as MJPEG or H.264 but are written as YUYV: it is what real
//! UVC cameras deliver, and every capture application accepts it.

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use remcam_codec::{Planes, h264, jpeg::JpegDecoder};
use remcam_proto::{Codec, VideoFormat};
use tracing::{debug, info, warn};
use v4l::Device;
use v4l::video::Output as _;
use v4l::video::output::Parameters;

const LOOPBACK_DRIVER: &str = "v4l2 loopback";
/// Frames waiting for the writer thread. MJPEG frames that are queued
/// behind newer ones are skipped; H.264 frames are all decoded (each one
/// is needed for the next) but only the newest is shown.
const QUEUE_LEN: usize = 8;
/// While waiting for a keyframe, ask again this often.
const KEYFRAME_RETRY: Duration = Duration::from_secs(1);

pub struct Frame {
    pub epoch: u32,
    pub codec: Codec,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Handle to the thread that decodes frames and writes them to the device.
pub struct Sink {
    tx: SyncSender<Frame>,
    thread: thread::JoinHandle<()>,
    pub format: VideoFormat,
    /// Frames received but not shown because newer ones were already waiting.
    pub dropped: Arc<AtomicU64>,
    /// Set when a frame never reached the decoder; H.264 then has to resync.
    lost: Arc<AtomicBool>,
    keyframe_wanted: Arc<AtomicBool>,
}

impl Sink {
    /// Configure `path` for `format`'s size and frame rate and start the writer thread.
    pub fn start(path: &str, format: VideoFormat) -> anyhow::Result<Self> {
        let writer = LoopbackWriter::open(path, format)?;
        let (tx, rx) = sync_channel::<Frame>(QUEUE_LEN);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = Arc::new(AtomicBool::new(false));
        let keyframe_wanted = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new().name("remcam-sink".into()).spawn({
            let mut decoder = Decoder::new(
                format,
                dropped.clone(),
                lost.clone(),
                keyframe_wanted.clone(),
            )?;
            move || {
                write_loop(rx, &mut decoder, writer);
                debug!("sink thread exiting");
            }
        })?;
        Ok(Self {
            tx,
            thread,
            format,
            dropped,
            lost,
            keyframe_wanted,
        })
    }

    /// Stop the writer and wait until the device is closed, so it can be
    /// reopened (e.g. with another size) right away.
    pub fn close(self) {
        drop(self.tx);
        let _ = self.thread.join();
    }

    /// Queue a frame for the writer. If the writer has fallen far behind,
    /// the frame is dropped instead.
    pub fn push(&self, frame: Frame) -> anyhow::Result<()> {
        match self.tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.mark_lost();
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => bail!("sink thread stopped"),
        }
    }

    /// Note that a frame never arrived, so the H.264 decoder must resync.
    pub fn mark_lost(&self) {
        self.lost.store(true, Ordering::Relaxed);
    }

    /// Whether the server should be asked for a keyframe (resets the flag).
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_wanted.swap(false, Ordering::Relaxed)
    }
}

fn write_loop(rx: Receiver<Frame>, decoder: &mut Decoder, mut writer: LoopbackWriter) {
    let mut next = None;
    loop {
        let frame = match next.take() {
            Some(f) => f,
            None => match rx.recv() {
                Ok(f) => f,
                Err(_) => return,
            },
        };
        // Only show a frame if no newer one is already waiting.
        next = rx.try_recv().ok();
        let show = next.is_none();
        if !show {
            decoder.dropped.fetch_add(1, Ordering::Relaxed);
        }
        match decoder.decode(frame, show) {
            Ok(true) => {
                if let Err(e) = writer.write(&decoder.yuyv) {
                    warn!("sink: {e:#}");
                }
            }
            Ok(false) => {}
            Err(e) => warn!("sink: {e:#}"),
        }
    }
}

/// Turns received frames of either codec into YUYV.
struct Decoder {
    width: usize,
    height: usize,
    jpeg: JpegDecoder,
    h264: h264::Decoder,
    /// Stream the H.264 decoder state belongs to.
    epoch: Option<u32>,
    /// H.264 lost sync; skip frames until the next keyframe.
    waiting_since: Option<Instant>,
    planes: Planes,
    yuyv: Vec<u8>,
    dropped: Arc<AtomicU64>,
    lost: Arc<AtomicBool>,
    keyframe_wanted: Arc<AtomicBool>,
}

impl Decoder {
    fn new(
        format: VideoFormat,
        dropped: Arc<AtomicU64>,
        lost: Arc<AtomicBool>,
        keyframe_wanted: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let (width, height) = (format.width as usize, format.height as usize);
        if !width.is_multiple_of(2) {
            bail!("YUYV needs an even width, got {width}");
        }
        Ok(Self {
            width,
            height,
            jpeg: JpegDecoder::new()?,
            h264: h264::Decoder::new()?,
            epoch: None,
            waiting_since: None,
            planes: Planes::default(),
            yuyv: vec![0; width * height * 2],
            dropped,
            lost,
            keyframe_wanted,
        })
    }

    /// Decode `frame`; if `show`, convert it into `self.yuyv`. Returns
    /// whether `self.yuyv` holds a new picture.
    fn decode(&mut self, frame: Frame, show: bool) -> anyhow::Result<bool> {
        match frame.codec {
            Codec::Mjpeg if !show => return Ok(false),
            Codec::Mjpeg => self.jpeg.decode(&frame.data, &mut self.planes)?,
            Codec::H264 => {
                if !self.h264_ready(&frame)? {
                    self.dropped.fetch_add(u64::from(show), Ordering::Relaxed);
                    return Ok(false);
                }
                match self.h264.decode(&frame.data, &mut self.planes) {
                    Ok(true) => {}
                    Ok(false) => return Ok(false),
                    Err(e) => {
                        self.resync();
                        return Err(e);
                    }
                }
                if !show {
                    return Ok(false);
                }
            }
        }
        let (w, h) = (self.planes.width, self.planes.height);
        if (w, h) != (self.width, self.height) {
            bail!("frame is {w}x{h}, expected {}x{}", self.width, self.height);
        }
        self.planes.to_yuyv(&mut self.yuyv);
        Ok(true)
    }

    /// Whether the H.264 decoder can take `frame`, handling new streams and
    /// lost frames.
    fn h264_ready(&mut self, frame: &Frame) -> anyhow::Result<bool> {
        if self.epoch != Some(frame.epoch) {
            // A new stream (e.g. after reconnecting) starts with a keyframe.
            self.h264 = h264::Decoder::new()?;
            self.epoch = Some(frame.epoch);
            self.waiting_since = None;
        }
        if self.lost.swap(false, Ordering::Relaxed) {
            self.resync();
        }
        let Some(since) = self.waiting_since else {
            return Ok(true);
        };
        if frame.keyframe {
            debug!("resynced after {:.0?}", since.elapsed());
            self.waiting_since = None;
            return Ok(true);
        }
        if since.elapsed() > KEYFRAME_RETRY {
            self.resync();
        }
        Ok(false)
    }

    fn resync(&mut self) {
        debug!("H.264 out of sync, requesting a keyframe");
        self.waiting_since = Some(Instant::now());
        self.keyframe_wanted.store(true, Ordering::Relaxed);
    }
}

struct LoopbackWriter {
    dev: Device,
}

impl LoopbackWriter {
    fn open(path: &str, format: VideoFormat) -> anyhow::Result<Self> {
        let dev = Device::with_path(path).with_context(|| format!("opening {path}"))?;
        let caps = dev.query_caps()?;
        if caps.driver != LOOPBACK_DRIVER {
            bail!(
                "{path} is not a v4l2loopback device (driver: {})",
                caps.driver
            );
        }

        let mut fmt = dev.format().context("reading output format")?;
        fmt.width = format.width;
        fmt.height = format.height;
        fmt.fourcc = v4l::FourCC::new(b"YUYV");
        fmt.stride = format.width * 2;
        fmt.size = format.width * format.height * 2;
        let fmt = dev.set_format(&fmt).with_context(|| {
            format!(
                "setting {path} to YUYV {}x{} (is another program writing to it?)",
                format.width, format.height
            )
        })?;
        if (fmt.width, fmt.height, &fmt.fourcc.repr) != (format.width, format.height, b"YUYV") {
            bail!(
                "{path} accepted {}x{} {} instead of YUYV {}x{}; reload v4l2loopback or close the apps using it",
                fmt.width,
                fmt.height,
                fmt.fourcc,
                format.width,
                format.height
            );
        }
        if let Err(e) = dev.set_params(&Parameters::with_fps(format.fps)) {
            // Only affects the rate apps are told about, not what we write.
            warn!("{path}: could not set frame rate: {e}");
        }
        info!(
            "{path}: writing YUYV {}x{}@{}",
            format.width, format.height, format.fps
        );
        Ok(Self { dev })
    }

    fn write(&mut self, yuyv: &[u8]) -> anyhow::Result<()> {
        write_all(self.dev.handle().fd(), yuyv).context("writing to loopback device")
    }
}

/// `write(2)` the whole buffer. The fd is non-blocking (as `v4l` opens it),
/// so wait for writability on `EAGAIN`.
fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: `buf` is a valid readable slice for its whole length.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n >= 0 {
            buf = &buf[n as usize..];
            continue;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: `pfd` is a valid pollfd and we pass a count of 1.
                if unsafe { libc::poll(&mut pfd, 1, 1000) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            _ => return Err(err),
        }
    }
    Ok(())
}
