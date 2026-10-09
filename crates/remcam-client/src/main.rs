//! remcam-client: receives a camera stream from remcam-server.
//!
//! Frames can be fed to a v4l2loopback device (`--sink`), so local apps see
//! the remote camera as a normal webcam, and/or saved as JPEG files (`--out`).
//! Throughput and latency are logged once per second.

mod decode;
mod sink;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::Parser;
use remcam_proto::tls::{self, ServerTrust};
use remcam_proto::{
    ClientMsg, DeviceInfo, FourCC, FrameHeader, MAX_FRAME_LEN, PROTOCOL_VERSION, SERVER_NAME,
    ServerMsg, VideoFormat, read_msg, write_msg,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

#[derive(Parser)]
#[command(about = "Receive a webcam stream from a remcam server")]
struct Args {
    /// Server address, as host or host:port.
    server: String,

    /// Device to open (e.g. /dev/video4). Defaults to the server's first camera.
    #[arg(long)]
    device: Option<String>,

    /// Frame size, WIDTHxHEIGHT.
    #[arg(long, default_value = "1280x720", value_parser = parse_size)]
    size: (u32, u32),

    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Expected SHA-256 fingerprint of the server certificate (printed by the server).
    #[arg(long, required_unless_present = "insecure")]
    fingerprint: Option<String>,

    /// Skip server authentication. The connection is still encrypted.
    #[arg(long, conflicts_with = "fingerprint")]
    insecure: bool,

    /// Feed frames to this v4l2loopback device (e.g. /dev/video10).
    #[arg(long, value_name = "DEVICE")]
    sink: Option<String>,

    /// Save each received frame as DIR/<seq>.jpg.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,

    /// Stop after receiving this many frames.
    #[arg(long)]
    frames: Option<u64>,
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    Ok((
        w.parse().map_err(|_| "bad width")?,
        h.parse().map_err(|_| "bad height")?,
    ))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let trust = match args.fingerprint.clone() {
        Some(fp) => ServerTrust::Fingerprint(fp),
        None => {
            warn!("--insecure: server identity is NOT verified");
            ServerTrust::Insecure
        }
    };
    let addr = resolve(&args.server).await?;
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()?;
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(tls::client_config(trust)?);

    let conn = endpoint
        .connect(addr, SERVER_NAME)?
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    let result = run(&conn, &args).await;
    conn.close(0u32.into(), b"bye");
    endpoint.wait_idle().await;
    result
}

async fn run(conn: &quinn::Connection, args: &Args) -> anyhow::Result<()> {
    let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
    write_msg(
        &mut ctl_tx,
        &ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            client_name: client_name(),
        },
    )
    .await?;

    match read_msg(&mut ctl_rx).await? {
        ServerMsg::Hello {
            version,
            server_name,
        } if version == PROTOCOL_VERSION => {
            info!("connected to {server_name} ({})", conn.remote_address());
        }
        ServerMsg::Hello { version, .. } => bail!("server speaks protocol version {version}"),
        ServerMsg::Error { msg } => bail!("server error: {msg}"),
        other => bail!("expected Hello, got {other:?}"),
    }
    let ServerMsg::DeviceList { devices } = read_msg(&mut ctl_rx).await? else {
        bail!("expected DeviceList");
    };
    for dev in &devices {
        info!("  camera {}: {}", dev.id, dev.name);
    }
    let device = pick_device(&devices, args.device.as_deref())?;

    let (width, height) = args.size;
    let format = VideoFormat {
        fourcc: FourCC::MJPG,
        width,
        height,
        fps: args.fps,
    };
    write_msg(
        &mut ctl_tx,
        &ClientMsg::Open {
            device_id: device.id.clone(),
            format,
        },
    )
    .await?;
    let (epoch, actual) = match read_msg(&mut ctl_rx).await? {
        ServerMsg::Opened { epoch, format } => {
            info!("streaming {} ({}) as {format}", device.id, device.name);
            (epoch, format)
        }
        ServerMsg::Error { msg } => bail!("server could not open {}: {msg}", device.id),
        other => bail!("expected Opened, got {other:?}"),
    };

    // Later control messages are only errors in M1; just log them.
    tokio::spawn(async move {
        while let Ok(msg) = read_msg::<_, ServerMsg>(&mut ctl_rx).await {
            warn!("server: {msg:?}");
        }
    });

    if let Some(dir) = &args.out {
        tokio::fs::create_dir_all(dir).await?;
    }
    let sink = args
        .sink
        .as_deref()
        .map(|path| sink::Sink::start(path, actual))
        .transpose()?;
    let sink_dropped = || {
        sink.as_ref()
            .map_or(0, |s| s.dropped.swap(0, Ordering::Relaxed))
    };

    let (frame_tx, mut frame_rx) = mpsc::channel(8);
    let stale = Arc::new(AtomicU64::new(0));
    tokio::spawn(accept_frames(conn.clone(), epoch, frame_tx, stale.clone()));

    let mut stats = Stats::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let mut received = 0u64;
    loop {
        tokio::select! {
            frame = frame_rx.recv() => {
                let Some((header, data)) = frame else { bail!("connection closed") };
                stats.record(&header, data.len());
                if let Some(dir) = &args.out {
                    tokio::fs::write(dir.join(format!("{:08}.jpg", header.seq)), &data).await?;
                }
                if let Some(sink) = &sink {
                    sink.push(data)?;
                }
                received += 1;
                if args.frames.is_some_and(|n| received >= n) {
                    break;
                }
            }
            _ = tick.tick() => stats.report(conn, stale.swap(0, Ordering::Relaxed), sink_dropped()),
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    stats.report(conn, stale.swap(0, Ordering::Relaxed), sink_dropped());
    write_msg(&mut ctl_tx, &ClientMsg::Close).await?;
    ctl_tx.finish()?;
    Ok(())
}

fn pick_device<'a>(
    devices: &'a [DeviceInfo],
    wanted: Option<&str>,
) -> anyhow::Result<&'a DeviceInfo> {
    match wanted {
        Some(id) => devices
            .iter()
            .find(|d| d.id == id)
            .with_context(|| format!("server has no camera {id}")),
        None => devices.first().context("server has no cameras"),
    }
}

/// Accept one unidirectional stream per frame and forward complete frames in
/// order of arrival. A frame that completes after a newer one has already
/// been delivered is stale and discarded.
async fn accept_frames(
    conn: quinn::Connection,
    epoch: u32,
    tx: mpsc::Sender<(FrameHeader, Vec<u8>)>,
    stale: Arc<AtomicU64>,
) {
    let newest = Arc::new(AtomicU64::new(0));
    while let Ok(mut stream) = conn.accept_uni().await {
        let (tx, newest, stale) = (tx.clone(), newest.clone(), stale.clone());
        tokio::spawn(async move {
            let header: FrameHeader = match read_msg(&mut stream).await {
                Ok(h) => h,
                Err(e) => return debug!("bad frame header: {e:#}"),
            };
            if header.epoch != epoch || header.seq < newest.load(Ordering::Relaxed) {
                stale.fetch_add(1, Ordering::Relaxed);
                let _ = stream.stop(0u32.into());
                return;
            }
            let data = match stream.read_to_end(MAX_FRAME_LEN).await {
                Ok(d) => d,
                Err(e) => return debug!("frame {} incomplete: {e}", header.seq),
            };
            if data.len() != header.len as usize {
                return warn!(
                    "frame {}: expected {} bytes, got {}",
                    header.seq,
                    header.len,
                    data.len()
                );
            }
            if newest.fetch_max(header.seq, Ordering::Relaxed) > header.seq {
                stale.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let _ = tx.send((header, data)).await;
        });
    }
}

#[derive(Default)]
struct Stats {
    frames: u64,
    bytes: u64,
    latency_sum_us: i64,
    latency_max_us: i64,
    last_seq: Option<u64>,
    /// Sequence numbers never delivered: dropped by the server, lost, or stale.
    missing: u64,
}

impl Stats {
    fn record(&mut self, header: &FrameHeader, len: usize) {
        let latency = now_us() as i64 - header.capture_ts_us as i64;
        self.frames += 1;
        self.bytes += len as u64;
        self.latency_sum_us += latency;
        self.latency_max_us = self.latency_max_us.max(latency);
        if let Some(last) = self.last_seq {
            self.missing += header.seq.saturating_sub(last + 1);
        }
        self.last_seq = Some(header.seq);
    }

    /// Log and reset the per-interval counters. Latency compares the server's
    /// and the client's wall clocks, so it is only meaningful when both are
    /// NTP-synced (or the same machine).
    fn report(&mut self, conn: &quinn::Connection, stale: u64, sink_dropped: u64) {
        let avg_ms = if self.frames > 0 {
            self.latency_sum_us as f64 / self.frames as f64 / 1000.0
        } else {
            0.0
        };
        info!(
            "{:>3} fps  {:>6.1} Mbit/s  latency avg {:>6.1} ms max {:>6.1} ms  rtt {:>5.1} ms  missing {} (stale {})  sink dropped {}",
            self.frames,
            self.bytes as f64 * 8.0 / 1e6,
            avg_ms,
            self.latency_max_us as f64 / 1000.0,
            conn.rtt().as_secs_f64() * 1000.0,
            self.missing,
            stale,
            sink_dropped,
        );
        *self = Stats {
            last_seq: self.last_seq,
            ..Default::default()
        };
    }
}

async fn resolve(server: &str) -> anyhow::Result<SocketAddr> {
    let port = remcam_proto::DEFAULT_PORT;
    if let Ok(addr) = server.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = server.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let with_port = match server.rsplit_once(':') {
        Some((_, p)) if p.parse::<u16>().is_ok() => server.to_string(),
        _ => format!("{server}:{port}"),
    };
    tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("resolving {with_port}"))?
        .next()
        .with_context(|| format!("no address for {with_port}"))
}

fn client_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "remcam-client".into())
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
