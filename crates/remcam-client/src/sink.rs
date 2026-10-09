//! Output to a v4l2loopback device, so that any V4L2 application sees the
//! remote camera as a local one.
//!
//! Frames arrive as MJPEG but are written as YUYV: it is what real UVC
//! cameras deliver, and unlike MJPEG every capture application accepts it.

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread;

use anyhow::{Context, bail};
use remcam_proto::VideoFormat;
use tracing::{debug, info, warn};
use v4l::Device;
use v4l::video::Output as _;
use v4l::video::output::Parameters;

use crate::decode::JpegToYuyv;

const LOOPBACK_DRIVER: &str = "v4l2 loopback";

/// Handle to the thread that decodes frames and writes them to the device.
pub struct Sink {
    tx: SyncSender<Vec<u8>>,
    /// Frames discarded because the decoder was still busy with the previous one.
    pub dropped: Arc<AtomicU64>,
}

impl Sink {
    /// Configure `path` for `format`'s size and frame rate and start the writer thread.
    pub fn start(path: &str, format: VideoFormat) -> anyhow::Result<Self> {
        let writer = LoopbackWriter::open(path, format)?;
        let (tx, rx) = sync_channel::<Vec<u8>>(1);
        let dropped = Arc::new(AtomicU64::new(0));
        thread::Builder::new()
            .name("remcam-sink".into())
            .spawn(move || {
                let mut writer = writer;
                for jpeg in rx {
                    if let Err(e) = writer.write_jpeg(&jpeg) {
                        warn!("sink: {e:#}");
                    }
                }
                debug!("sink thread exiting");
            })?;
        Ok(Self { tx, dropped })
    }

    /// Hand a frame to the writer, dropping it if the writer is still busy:
    /// showing the newest frame matters more than showing every frame.
    pub fn push(&self, jpeg: Vec<u8>) -> anyhow::Result<()> {
        match self.tx.try_send(jpeg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => bail!("sink thread stopped"),
        }
    }
}

struct LoopbackWriter {
    dev: Device,
    decoder: JpegToYuyv,
    yuyv: Vec<u8>,
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
        let (width, height) = (format.width as usize, format.height as usize);
        let decoder = JpegToYuyv::new(width, height)?;

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

        Ok(Self {
            dev,
            decoder,
            yuyv: vec![0; width * height * 2],
        })
    }

    fn write_jpeg(&mut self, jpeg: &[u8]) -> anyhow::Result<()> {
        self.decoder.decode(jpeg, &mut self.yuyv)?;
        write_all(self.dev.handle().fd(), &self.yuyv).context("writing to loopback device")
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
