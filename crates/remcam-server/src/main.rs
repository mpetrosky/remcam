//! remcam-server: shares local V4L2 cameras over QUIC.

mod auth;
mod camera;
mod limits;
mod mdns;
mod pipeline;
mod rate;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use remcam_proto::{
    ClientMsg, Codec, PROTOCOL_VERSION, ServerMsg, read_msg, tls, trust, write_msg,
};
use tokio::sync::OwnedSemaphorePermit;
use tracing::{debug, info, warn};

use crate::auth::Auth;
use crate::camera::Controls;
use crate::limits::{Limits, Refused};
use crate::pipeline::Pipeline;

/// How long a new connection may take to say Hello (or Pair).
const FIRST_MSG_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum gap between warnings that the address table is full, so a sustained
/// flood doesn't flood the log too.
const TABLE_FULL_WARN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(about = "Share local webcams over the network")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// UDP address to listen on.
    #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], remcam_proto::DEFAULT_PORT)))]
    listen: SocketAddr,

    /// Directory holding the server certificate and key (created if missing)
    /// and authorized_clients. Defaults to $XDG_CONFIG_HOME/remcam or
    /// ~/.config/remcam.
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    /// Print the available cameras, their formats and controls, and exit.
    #[arg(long)]
    list: bool,

    /// Don't advertise this server on the LAN via mDNS.
    #[arg(long)]
    no_mdns: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Show a one-time code to pair a new client with the running server.
    Pair,
    /// List the clients allowed to connect.
    Clients,
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
        None => trust::default_config_dir()?,
    };
    let server_name = hostname();
    match args.command {
        Some(Command::Pair) => return auth::run_pair_command(&config_dir, &server_name).await,
        Some(Command::Clients) => {
            let clients = trust::AuthorizedClients::new(&config_dir);
            let entries = clients.entries()?;
            if entries.is_empty() {
                println!("no clients are authorized yet; pair one with `remcam-server pair`");
            }
            for e in entries {
                println!("{:<24} {}", e.name, e.fingerprint);
            }
            return Ok(());
        }
        None => {}
    }

    let identity = tls::load_or_create_identity(&config_dir)?;
    let fingerprint = identity.fingerprint();
    let endpoint = quinn::Endpoint::server(tls::server_config(identity)?, args.listen)
        .with_context(|| format!("binding {}", args.listen))?;

    info!("listening on udp/{}", endpoint.local_addr()?);
    info!("certificate fingerprint: {fingerprint}");

    let auth = Arc::new(Auth::new(
        &config_dir,
        fingerprint.clone(),
        server_name.clone(),
    ));
    match auth.clients.entries() {
        Ok(e) if e.is_empty() => {
            info!("no clients are authorized yet; run `remcam-server pair` to add one")
        }
        Ok(e) => info!(
            "{} authorized client(s) in {}",
            e.len(),
            auth.clients.path().display()
        ),
        Err(e) => warn!("{e:#}"),
    }
    if let Err(e) = auth::listen(&config_dir, auth.clone()) {
        warn!("pairing unavailable: {e:#}");
    }
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
    let limits = Limits::new();
    let mut last_full_warn: Option<Instant> = None;
    while let Some(incoming) = endpoint.accept().await {
        let remote = incoming.remote_address();
        // Make the client prove it owns its source address before it counts
        // against any limit; otherwise spoofed packets can fill the table.
        if !incoming.remote_address_validated() {
            if let Err(e) = incoming.retry() {
                e.into_incoming().ignore();
            }
            continue;
        }
        let now = Instant::now();
        let permit = match limits.admit(remote.ip(), now) {
            Ok(permit) => permit,
            Err(Refused::TableFull) => {
                if last_full_warn.is_none_or(|t| now.duration_since(t) >= TABLE_FULL_WARN_INTERVAL)
                {
                    warn!(
                        "too many client addresses connecting; refusing new ones ({remote} and others)"
                    );
                    last_full_warn = Some(now);
                } else {
                    debug!("{remote}: address table full, refusing");
                }
                incoming.refuse();
                continue;
            }
            Err(reason) => {
                debug!("{remote}: too many connections ({reason:?}), refusing");
                incoming.refuse();
                continue;
            }
        };
        let auth = auth.clone();
        tokio::spawn(async move {
            match handle_connection(incoming, auth, permit).await {
                Ok(()) => info!("{remote}: disconnected"),
                Err(e) => warn!("{remote}: {e:#}"),
            }
        });
    }
    Ok(())
}

async fn handle_connection(
    incoming: quinn::Incoming,
    auth: Arc<Auth>,
    unauthenticated: OwnedSemaphorePermit,
) -> anyhow::Result<()> {
    let conn = incoming.await?;
    let remote = conn.remote_address();
    let client_fp = tls::peer_fingerprint(&conn).context("client presented no certificate")?;
    let first = async {
        let (ctl_tx, mut ctl_rx) = conn.accept_bi().await?;
        let msg: ClientMsg = read_msg(&mut ctl_rx).await?;
        anyhow::Ok((ctl_tx, ctl_rx, msg))
    };
    let (mut ctl_tx, mut ctl_rx, first) = tokio::time::timeout(FIRST_MSG_TIMEOUT, first)
        .await
        .context("no Hello from client")??;

    // Client-chosen names go into logs and authorized_clients; keep them tame.
    let first = match first {
        ClientMsg::Hello {
            version,
            client_name,
        } => ClientMsg::Hello {
            version,
            client_name: trust::sanitize_name(&client_name),
        },
        ClientMsg::Pair {
            version,
            client_name,
            spake,
        } => ClientMsg::Pair {
            version,
            client_name: trust::sanitize_name(&client_name),
            spake,
        },
        other => other,
    };
    match first {
        ClientMsg::Hello {
            version,
            client_name,
        } if version == PROTOCOL_VERSION => match auth.clients.lookup(&client_fp)? {
            Some(name) if name == client_name => {
                info!("{remote}: connected ({name})")
            }
            Some(name) => info!("{remote}: connected ({client_name}, authorized as {name})"),
            None => {
                let msg = ServerMsg::Unauthorized {
                    fingerprint: client_fp.clone(),
                };
                auth::send_final(&mut ctl_tx, &msg).await;
                bail!("rejected unknown client {client_name} ({client_fp})");
            }
        },
        ClientMsg::Hello { version, .. } | ClientMsg::Pair { version, .. }
            if version != PROTOCOL_VERSION =>
        {
            let msg =
                format!("unsupported protocol version {version}, server speaks {PROTOCOL_VERSION}");
            auth::send_final(&mut ctl_tx, &ServerMsg::Error { msg: msg.clone() }).await;
            bail!(msg);
        }
        ClientMsg::Pair {
            client_name, spake, ..
        } => {
            info!("{remote}: pairing attempt from {client_name}");
            return auth::pair(
                &auth,
                &conn,
                &mut ctl_tx,
                &mut ctl_rx,
                &client_fp,
                &client_name,
                &spake,
            )
            .await;
        }
        other => bail!("expected Hello, got {other:?}"),
    }
    drop(unauthenticated);
    write_msg(
        &mut ctl_tx,
        &ServerMsg::Hello {
            version: PROTOCOL_VERSION,
            server_name: auth.server_name.clone(),
            codecs: vec![Codec::Mjpeg, Codec::H264],
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

    let mut active: Option<(Pipeline, Arc<Controls>)> = None;
    let mut epoch = 0u32;
    // The control stream ending (or the connection dropping) ends the session.
    while let Ok(msg) = read_msg::<_, ClientMsg>(&mut ctl_rx).await {
        match msg {
            ClientMsg::Open {
                device_id,
                format,
                stream,
            } => {
                if let Some((pipeline, _)) = active.take() {
                    pipeline.stop().await;
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
                match Pipeline::start(conn.clone(), epoch, device_id.clone(), format, stream).await
                {
                    Ok((pipeline, actual, stream)) => {
                        match stream.max_bitrate {
                            Some(max) => info!(
                                "{remote}: streaming {device_id} ({actual}) as {}, up to {:.2} Mbit/s",
                                stream.codec,
                                max as f64 / 1e6
                            ),
                            None => info!(
                                "{remote}: streaming {device_id} ({actual}) as {}",
                                stream.codec
                            ),
                        }
                        write_msg(
                            &mut ctl_tx,
                            &ServerMsg::Opened {
                                epoch,
                                format: actual,
                                stream,
                            },
                        )
                        .await?;
                        send_control_states(&mut ctl_tx, &controls).await?;
                        active = Some((pipeline, controls));
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
            ClientMsg::RequestKeyframe => {
                if let Some((pipeline, _)) = &active {
                    pipeline.request_keyframe();
                }
            }
            ClientMsg::Close => {
                if let Some((pipeline, _)) = active.take() {
                    pipeline.stop().await;
                }
            }
            ClientMsg::Hello { .. } | ClientMsg::Pair { .. } | ClientMsg::PairConfirm { .. } => {
                let msg = "unexpected message".to_string();
                write_msg(&mut ctl_tx, &ServerMsg::Error { msg }).await?;
            }
        }
    }
    if let Some((pipeline, _)) = active.take() {
        pipeline.stop().await;
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

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "remcam".into())
}
