//! remcam-server: shares local V4L2 cameras over QUIC.

mod camera;
mod mdns;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Parser;
use remcam_proto::{ClientMsg, FrameHeader, PROTOCOL_VERSION, ServerMsg, read_msg, tls, write_msg};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};

use crate::camera::{Capture, Controls, Frame};

/// Frames handed to QUIC but not yet fully written. Beyond this, new frames
/// are dropped instead of queued, which bounds latency when the link is slow.
const MAX_INFLIGHT_FRAMES: usize = 4;

#[derive(Parser)]
#[command(about = "Share local webcams over the network")]
struct Args {
    /// UDP address to listen on.
    #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], remcam_proto::DEFAULT_PORT)))]
    listen: SocketAddr,

    /// Directory holding the server certificate and key (created if missing).
    /// Defaults to $XDG_CONFIG_HOME/remcam or ~/.config/remcam.
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Print the available cameras, their formats and controls, and exit.
    #[arg(long)]
    list: bool,

    /// Don't advertise this server on the LAN via mDNS.
    #[arg(long)]
    no_mdns: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    if args.list {
        print_devices(&camera::list_devices());
        return Ok(());
    }

    let config_dir = match args.config_dir {
        Some(d) => d,
        None => default_config_dir()?,
    };
    let (cert, key) = tls::load_or_create_identity(&config_dir)?;
    let fingerprint = tls::fingerprint(&cert);
    let endpoint = quinn::Endpoint::server(tls::server_config(cert, key)?, args.listen)
        .with_context(|| format!("binding {}", args.listen))?;

    info!("listening on udp/{}", endpoint.local_addr()?);
    info!("certificate fingerprint: {fingerprint}");

    let server_name = hostname();
    // Kept alive for the lifetime of the server; dropping it stops advertising.
    let _mdns = if args.no_mdns {
        None
    } else {
        match mdns::advertise(&server_name, endpoint.local_addr()?, &fingerprint) {
            Ok(daemon) => daemon,
            Err(e) => {
                warn!("mDNS advertisement failed: {e:#}");
                None
            }
        }
    };
    while let Some(incoming) = endpoint.accept().await {
        let server_name = server_name.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match handle_connection(incoming, server_name).await {
                Ok(()) => info!("{remote}: disconnected"),
                Err(e) => warn!("{remote}: {e:#}"),
            }
        });
    }
    Ok(())
}

async fn handle_connection(incoming: quinn::Incoming, server_name: String) -> anyhow::Result<()> {
    let conn = incoming.await?;
    let remote = conn.remote_address();
    let (mut ctl_tx, mut ctl_rx) = conn.accept_bi().await?;

    match read_msg(&mut ctl_rx).await? {
        ClientMsg::Hello {
            version,
            client_name,
        } if version == PROTOCOL_VERSION => {
            info!("{remote}: connected ({client_name})");
        }
        ClientMsg::Hello { version, .. } => {
            let msg =
                format!("unsupported protocol version {version}, server speaks {PROTOCOL_VERSION}");
            write_msg(&mut ctl_tx, &ServerMsg::Error { msg: msg.clone() }).await?;
            bail!(msg);
        }
        other => bail!("expected Hello, got {other:?}"),
    }
    write_msg(
        &mut ctl_tx,
        &ServerMsg::Hello {
            version: PROTOCOL_VERSION,
            server_name,
        },
    )
    .await?;

    let devices = tokio::task::spawn_blocking(camera::list_devices).await?;
    write_msg(
        &mut ctl_tx,
        &ServerMsg::DeviceList {
            devices: devices.clone(),
        },
    )
    .await?;

    let mut active: Option<(Capture, Arc<Controls>)> = None;
    let mut epoch = 0u32;
    // The control stream ending (or the connection dropping) ends the session.
    while let Ok(msg) = read_msg::<_, ClientMsg>(&mut ctl_rx).await {
        match msg {
            ClientMsg::Open { device_id, format } => {
                if let Some((capture, _)) = active.take() {
                    capture.stop().await;
                }
                // Only devices we advertised may be opened; never an arbitrary path.
                let Some(device) = devices.iter().find(|d| d.id == device_id) else {
                    let msg = format!("unknown device {device_id}");
                    write_msg(&mut ctl_tx, &ServerMsg::Error { msg }).await?;
                    continue;
                };
                let controls = match Controls::open(&device_id, device.controls.clone()) {
                    Ok(c) => Arc::new(c),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        write_msg(&mut ctl_tx, &ServerMsg::Error { msg }).await?;
                        continue;
                    }
                };
                epoch += 1;
                let (tx, rx) = mpsc::channel(1);
                match Capture::start(device_id.clone(), format, tx).await {
                    Ok((capture, actual)) => {
                        info!("{remote}: streaming {device_id} as {actual}");
                        tokio::spawn(send_frames(conn.clone(), epoch, rx));
                        write_msg(
                            &mut ctl_tx,
                            &ServerMsg::Opened {
                                epoch,
                                format: actual,
                            },
                        )
                        .await?;
                        send_control_states(&mut ctl_tx, &controls).await?;
                        active = Some((capture, controls));
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        warn!("{remote}: open {device_id} failed: {msg}");
                        write_msg(&mut ctl_tx, &ServerMsg::Error { msg }).await?;
                    }
                }
            }
            ClientMsg::SetControl { id, value } => {
                let Some((_, controls)) = &active else {
                    let msg = "no device open".to_string();
                    write_msg(&mut ctl_tx, &ServerMsg::Error { msg }).await?;
                    continue;
                };
                let c = controls.clone();
                match tokio::task::spawn_blocking(move || c.set(id, value)).await? {
                    Ok(()) => debug!("{remote}: control {id:#x} = {value}"),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        write_msg(&mut ctl_tx, &ServerMsg::Error { msg }).await?;
                    }
                }
                // Report the outcome either way, so the client stays in sync.
                send_control_states(&mut ctl_tx, controls).await?;
            }
            ClientMsg::Close => {
                if let Some((capture, _)) = active.take() {
                    capture.stop().await;
                }
            }
            ClientMsg::Hello { .. } => {
                let msg = "unexpected Hello".to_string();
                write_msg(&mut ctl_tx, &ServerMsg::Error { msg }).await?;
            }
        }
    }
    if let Some((capture, _)) = active.take() {
        capture.stop().await;
    }
    Ok(())
}

async fn send_control_states(
    ctl_tx: &mut quinn::SendStream,
    controls: &Arc<Controls>,
) -> anyhow::Result<()> {
    let c = controls.clone();
    match tokio::task::spawn_blocking(move || c.states()).await? {
        Ok(controls) => write_msg(ctl_tx, &ServerMsg::Controls { controls }).await,
        Err(e) => {
            warn!("reading controls failed: {e:#}");
            Ok(())
        }
    }
}

fn print_devices(devices: &[remcam_proto::DeviceInfo]) {
    for dev in devices {
        print!("{}", dev.describe());
    }
}

/// Send each captured frame on its own unidirectional stream.
async fn send_frames(conn: quinn::Connection, epoch: u32, mut rx: mpsc::Receiver<Frame>) {
    let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_FRAMES));
    let mut dropped = 0u64;
    while let Some(frame) = rx.recv().await {
        let Ok(permit) = inflight.clone().try_acquire_owned() else {
            dropped += 1;
            if dropped.is_power_of_two() {
                debug!("link busy, {dropped} frames dropped so far");
            }
            continue;
        };
        let conn = conn.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = send_frame(&conn, epoch, frame).await {
                debug!("frame send failed: {e:#}");
            }
        });
    }
}

async fn send_frame(conn: &quinn::Connection, epoch: u32, frame: Frame) -> anyhow::Result<()> {
    let mut stream = conn.open_uni().await?;
    let header = FrameHeader {
        epoch,
        seq: frame.seq,
        capture_ts_us: frame.capture_ts_us,
        len: frame.data.len() as u32,
    };
    write_msg(&mut stream, &header).await?;
    stream.write_all(&frame.data).await?;
    stream.finish()?;
    Ok(())
}

fn default_config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("remcam"));
    }
    let home = std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".config/remcam"))
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "remcam".into())
}
